//! Owns the maker's lifecycle: starting it, stopping it inside a deadline, and reporting what
//! it is doing.
//!
//! Two coinswap properties drive the design: `MakerServer::init` can hang for tens of seconds,
//! so it runs on the maker's own thread and errors surface on the dashboard rather than failing
//! `Plugin::start`; and `start_server` blocks for the maker's whole life, so it is the thread
//! body rather than a call that returns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::shared::{
    describe, lock, sleep_unless_stopped, Logger, Phase, SendOnDrop, RETRY_BACKOFF, START_ATTEMPTS,
};
use coinswap::maker::{start_server, MakerServer, MakerServerConfig};
use coinswap::utill::check_tor_status;
use coinswap::wallet::{AddressType, Balances};

/// State both the maker thread and the plugin touch.
///
/// One lock rather than one per field, so a phase cannot disagree with the handle beside it.
struct Shared {
    phase: Phase,
    /// `None` until init finishes, which is why a stop arriving mid-init needs `stop_requested`
    /// as well as this.
    maker: Option<Arc<MakerServer>>,
    /// A newly created wallet's seed phrase, held for one display and then dropped.
    ///
    /// Memory only: never logged, persisted, or written to the settings store. A restart before
    /// the operator reads it loses it, which is the right direction to fail in.
    new_mnemonic: Option<String>,
}

/// Thread bookkeeping, touched only by `start` and `stop`.
struct Control {
    stop_requested: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Fires when the thread body has returned, so a join can be bounded by a deadline.
    finished: Receiver<()>,
}

/// Starts and stops the maker, and answers questions about it.
pub struct MakerRuntime {
    shared: Arc<Mutex<Shared>>,
    control: Mutex<Option<Control>>,
}

impl MakerRuntime {
    /// A runtime with no maker running.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                phase: Phase::Stopped,
                maker: None,
                new_mnemonic: None,
            })),
            control: Mutex::new(None),
        }
    }

    /// What the maker is doing, without querying it.
    pub fn phase(&self) -> Phase {
        lock(&self.shared).phase.clone()
    }

    /// True when a maker is starting or running.
    pub fn is_live(&self) -> bool {
        matches!(self.phase(), Phase::Starting | Phase::Running)
    }

    /// Returns a new wallet's seed phrase the first time it is asked for, and nothing after.
    ///
    /// Takes rather than reads, so a page reload cannot put it back on screen.
    pub fn take_new_mnemonic(&self) -> Option<String> {
        lock(&self.shared).new_mnemonic.take()
    }

    /// Starts a maker, unless one is already live.
    ///
    /// Returns as soon as the thread is spawned; a backend that cannot be reached shows up
    /// later as [`Phase::Failed`].
    pub fn start(&self, config: MakerServerConfig, log: Logger) -> Result<(), String> {
        let mut control = lock(&self.control);
        if self.is_live() {
            return Err("The maker is already running.".to_string());
        }

        let stop_requested = Arc::new(AtomicBool::new(false));
        let (finished_tx, finished_rx) = mpsc::channel();

        {
            let mut shared = lock(&self.shared);
            shared.phase = Phase::Starting;
            shared.maker = None;
        }

        let thread = std::thread::Builder::new()
            .name("coinswap-maker".to_string())
            .spawn({
                let shared = Arc::clone(&self.shared);
                let stop_requested = Arc::clone(&stop_requested);
                move || {
                    // Fires on any exit including a panic, so `stop` never waits out the full
                    // deadline for a thread that is already gone.
                    let _signal = SendOnDrop(finished_tx);

                    let maker = match MakerServer::init(config) {
                        Ok(maker) => Arc::new(maker),
                        Err(error) => {
                            let message = describe(error);
                            log.error(&format!("Maker failed to start: {message}"));
                            lock(&shared).phase = Phase::Failed(message);
                            return;
                        }
                    };

                    // Init creates the wallet, so this is the only place the phrase appears.
                    match maker.wallet.write() {
                        Ok(mut wallet) => {
                            if let Some(mnemonic) = wallet.take_new_mnemonic() {
                                lock(&shared).new_mnemonic = Some(mnemonic.words());
                                log.info(
                                    "A new maker wallet was created. Its recovery phrase is on \
                                     the dashboard and is shown only once.",
                                );
                            }
                        }
                        Err(_) => log.error(
                            "Could not read the maker wallet after starting it. If this was a \
                             new wallet, its recovery phrase could not be shown.",
                        ),
                    }

                    lock(&shared).maker = Some(Arc::clone(&maker));

                    // A stop during init had no server to signal, so honour it here.
                    if stop_requested.load(Ordering::Relaxed) {
                        log.info("Maker stopped before it finished starting.");
                        lock(&shared).phase = Phase::Stopped;
                        return;
                    }

                    // Only the Tor check is retried; `start_server` is then called exactly once
                    // because it is not idempotent -- retrying it leaves several watchtowers
                    // running against one wallet.
                    let mut tor = Err("not checked".to_string());
                    for attempt in 1..=START_ATTEMPTS {
                        if stop_requested.load(Ordering::Relaxed) {
                            lock(&shared).phase = Phase::Stopped;
                            return;
                        }

                        // coinswap reports "Failed to retrieve ephemeral onion service details"
                        // whether Tor is absent, rejected the password, or refused the key. This
                        // separates those cases.
                        tor = check_tor_status(
                            maker.config.control_port,
                            &maker.config.tor_auth_password,
                        )
                        .map_err(|error| {
                            format!(
                                "Tor is not usable on control port {}: {}. Check the Tor control \
                                 port and password on the settings page; the password must match \
                                 the one Tor was configured with.",
                                maker.config.control_port,
                                describe(error)
                            )
                        });

                        if tor.is_ok() {
                            break;
                        }

                        if attempt < START_ATTEMPTS {
                            let wait = RETRY_BACKOFF * attempt;
                            log.error(&format!(
                                "Tor check {attempt} of {START_ATTEMPTS} failed: {}. Retrying in \
                                 {}s.",
                                tor.as_ref().err().map_or("", String::as_str),
                                wait.as_secs()
                            ));
                            if sleep_unless_stopped(wait, &stop_requested) {
                                lock(&shared).phase = Phase::Stopped;
                                return;
                            }
                        }
                    }

                    // `start_server` only returns on failure, so setting Running before the call
                    // is the closest thing to a "started" signal coinswap offers.
                    let outcome = match tor {
                        Ok(()) => {
                            lock(&shared).phase = Phase::Running;
                            log.info("Maker is running.");
                            start_server(Arc::clone(&maker)).map_err(describe)
                        }
                        Err(message) => Err(message),
                    };

                    // A requested shutdown surfaces as an error here, so it only counts as a
                    // failure if nobody asked to stop.
                    let asked_to_stop =
                        stop_requested.load(Ordering::Relaxed) || maker.is_shutdown();
                    let mut shared = lock(&shared);
                    shared.phase = match outcome {
                        Err(message) if !asked_to_stop => {
                            log.error(&format!("Maker stopped serving: {message}"));
                            // Kept rather than cleared: init succeeded, so the wallet is open
                            // and still the only way to read a balance or hand out an address --
                            // which is exactly what an empty wallet needs.
                            Phase::Failed(message)
                        }
                        _ => {
                            shared.maker = None;
                            log.info("Maker stopped.");
                            Phase::Stopped
                        }
                    };
                }
            })
            .map_err(|error| {
                lock(&self.shared).phase = Phase::Stopped;
                format!("Could not spawn the maker thread: {error}")
            })?;

        *control = Some(Control {
            stop_requested,
            thread: Some(thread),
            finished: finished_rx,
        });

        Ok(())
    }

    /// Signals the maker to stop and waits up to `deadline` for its thread to finish.
    ///
    /// `false` means it was left detached: leaking a thread beats blocking BTCPay's shutdown.
    /// Never panics, because it runs on the shutdown path.
    pub fn stop(&self, deadline: Duration, log: &Logger) -> bool {
        let mut control = lock(&self.control);
        let Some(control) = control.as_mut() else {
            return true;
        };

        control.stop_requested.store(true, Ordering::Relaxed);

        // Release the lock before waiting, so a dashboard render is not blocked for the
        // whole deadline.
        let maker = lock(&self.shared).maker.clone();
        if let Some(maker) = maker {
            maker.shutdown.store(true, Ordering::Relaxed);
        }

        let stopped = match control.finished.recv_timeout(deadline) {
            // The body has returned, so this join is immediate.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(handle) = control.thread.take() {
                    let _ = handle.join();
                }
                true
            }
            Err(RecvTimeoutError::Timeout) => {
                log.error(&format!(
                    "The maker did not stop within {}s, so it has been left detached.",
                    deadline.as_secs()
                ));
                false
            }
        };

        if stopped {
            let mut shared = lock(&self.shared);
            shared.maker = None;
            // Preserve a failure so the dashboard can still explain why the maker went away.
            if !matches!(shared.phase, Phase::Failed(_)) {
                shared.phase = Phase::Stopped;
            }
        }

        // No phase reset when it did not stop: the detached thread sets it on exit, and until
        // then `is_live` stays true, which is what stops a second maker binding the same port.
        stopped
    }

    /// A point-in-time view of the maker for the dashboard.
    ///
    /// Every query may fail into `None`: rendering one field as unavailable beats erroring the
    /// whole page.
    pub fn status(&self) -> Status {
        let (phase, maker) = {
            let shared = lock(&self.shared);
            (shared.phase.clone(), shared.maker.clone())
        };

        let Some(maker) = maker else {
            return Status::without_maker(phase);
        };

        // `try_read`, never `read`: coinswap holds the wallet's write lock across a chain sync
        // while waiting for the bond to be funded, which is indefinite on an unfunded maker.
        let (balances, bonds, wallet_busy) = match maker.wallet.try_read() {
            Ok(wallet) => (
                wallet.get_balances().ok(),
                wallet
                    .get_fidelity_bonds()
                    .iter()
                    .map(|bond| (bond.amount.to_sat(), bond.lock_time.to_string()))
                    .collect(),
                false,
            ),
            Err(_) => (None, Vec::new(), true),
        };

        // No `check_swap_liquidity` here: it makes RPC calls, and a page render must not.
        // `try_lock` again: the maker holds this while it works a swap.
        let swaps = match maker.swap_tracker.try_lock() {
            Ok(tracker) => tracker
                .incomplete_swaps()
                .into_iter()
                .map(|record| SwapRecord {
                    id: record.swap_id.chars().take(16).collect(),
                    phase: format!("{:?}", record.phase),
                    recovery: format!("{:?}", record.recovery.phase),
                    amount_sat: record.swap_amount_sat,
                    funded: record.funding_broadcast,
                })
                .collect(),
            Err(_) => Vec::new(),
        };

        Status {
            phase,
            balances,
            // Cheap: a mutex around a collection, no I/O.
            ongoing_swaps: maker.has_ongoing_swaps().ok(),
            bonds,
            wallet_busy,
            port: Some(maker.config.network_port),
            swaps,
        }
    }

    /// True when the wallet is open and can be queried, which outlasts a failed start.
    pub fn wallet_open(&self) -> bool {
        lock(&self.shared).maker.is_some()
    }

    /// Hands out a fresh address for funding the maker's wallet.
    ///
    /// A command, not a dashboard field: it advances the address index and writes to disk.
    pub fn receive_address(&self) -> Result<String, String> {
        let maker = lock(&self.shared).maker.clone();
        let Some(maker) = maker else {
            return Err(
                "The maker's wallet is not open yet. Start the maker, and if it fails, its \
                 wallet stays open so this will work."
                    .to_string(),
            );
        };

        // `try_write` for the same reason `status` uses `try_read`.
        let mut wallet = maker.wallet.try_write().map_err(|_| {
            "The maker is busy syncing its wallet, so it cannot derive an address right now. \
             coinswap logs the address it wants funded to BTCPay's log, which needs no lock."
                .to_string()
        })?;

        wallet
            .get_next_external_address(AddressType::P2WPKH)
            .map(|address| address.to_string())
            .map_err(|error| format!("Could not derive a receive address: {}", describe(error)))
    }

    /// Clears out swaps that are idle past `timeout`, returning how many went.
    pub fn drain_idle_swaps(&self, timeout: Duration) -> Result<usize, String> {
        let maker = lock(&self.shared).maker.clone();
        let Some(maker) = maker else {
            return Err("The maker is not running.".to_string());
        };

        maker
            .drain_idle_swaps(timeout)
            .map(|drained| drained.len())
            .map_err(|error| format!("Could not drain idle swaps: {}", describe(error)))
    }
}

impl Default for MakerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// One swap the maker has a record of.
///
/// Owned rather than borrowed: the tracker sits behind a mutex the maker itself takes.
pub struct SwapRecord {
    /// Short identifier, as coinswap logs it.
    pub id: String,
    pub phase: String,
    pub recovery: String,
    pub amount_sat: u64,
    /// Whether a funding transaction reached the chain.
    ///
    /// If false there is nothing on-chain to recover.
    pub funded: bool,
}

impl SwapRecord {
    /// True when this swap still has funds on chain that recovery has not finished returning.
    pub fn funds_at_stake(&self) -> bool {
        self.funded && self.recovery != "CleanedUp" && self.phase != "Completed"
    }
}

/// A point-in-time view of the maker, cheap enough to build on every page load.
///
/// A snapshot, so rendering never holds the wallet lock while the maker wants it.
pub struct Status {
    pub phase: Phase,
    /// Wallet balances, absent when the maker is down or the wallet lock was busy.
    pub balances: Option<Balances>,
    /// Whether a swap is in flight. `None` when it could not be determined.
    pub ongoing_swaps: Option<bool>,
    /// Fidelity bonds held, as (amount in sats, locktime).
    pub bonds: Vec<(u64, String)>,
    /// True when the wallet was locked, so balances and bonds could not be read.
    ///
    /// Not an error: normal for seconds at a time, and continuous while waiting to be funded.
    pub wallet_busy: bool,
    /// The port takers reach this maker on, when it is up.
    pub port: Option<u16>,
    /// Swaps the maker has records for, newest first. Empty when it has none or was busy.
    pub swaps: Vec<SwapRecord>,
}

impl Status {
    fn without_maker(phase: Phase) -> Self {
        Self {
            phase,
            balances: None,
            ongoing_swaps: None,
            bonds: Vec::new(),
            wallet_busy: false,
            port: None,
            swaps: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_runtime_is_stopped_and_has_no_seed_phrase() {
        let runtime = MakerRuntime::new();
        assert_eq!(runtime.phase(), Phase::Stopped);
        assert!(!runtime.is_live());
        assert!(runtime.take_new_mnemonic().is_none());
    }

    #[test]
    fn stopping_a_runtime_that_never_started_succeeds() {
        // `stop` runs on BTCPay's shutdown path, where panicking is not an option.
        let runtime = MakerRuntime::new();
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
    }

    #[test]
    fn stopping_twice_succeeds() {
        let runtime = MakerRuntime::new();
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
    }

    #[test]
    fn commands_report_that_the_wallet_is_shut_rather_than_panicking() {
        // The dashboard is reachable while the maker is stopped, so commands must survive that.
        let runtime = MakerRuntime::new();
        assert!(runtime.receive_address().is_err());
        assert!(runtime.drain_idle_swaps(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn a_status_without_a_maker_still_renders() {
        let runtime = MakerRuntime::new();
        let status = runtime.status();
        assert_eq!(status.phase, Phase::Stopped);
        assert!(status.balances.is_none());
        assert!(status.port.is_none());
        assert!(status.bonds.is_empty());
    }

    #[test]
    fn a_seed_phrase_is_handed_out_once() {
        let runtime = MakerRuntime::new();
        lock(&runtime.shared).new_mnemonic = Some("abandon abandon about".to_string());

        assert!(runtime.take_new_mnemonic().is_some());
        assert!(
            runtime.take_new_mnemonic().is_none(),
            "a reload must not put the phrase back on screen"
        );
    }

    #[test]
    fn a_poisoned_lock_does_not_wedge_the_runtime() {
        // Otherwise a poisoned lock would make `stop` fail and the maker unstoppable.
        let runtime = MakerRuntime::new();
        let shared = Arc::clone(&runtime.shared);
        let _ = std::thread::spawn(move || {
            let _guard = shared.lock().unwrap();
            panic!("poison it");
        })
        .join();

        assert_eq!(runtime.phase(), Phase::Stopped);
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
    }

    #[test]
    fn a_failure_survives_a_later_stop() {
        // Overwriting the reason with a bare "stopped" loses the only clue the operator had.
        let runtime = MakerRuntime::new();
        lock(&runtime.shared).phase = Phase::Failed("backend unreachable".to_string());

        runtime.stop(Duration::from_secs(1), &Logger::silent());

        assert_eq!(
            runtime.phase(),
            Phase::Failed("backend unreachable".to_string())
        );
    }

    #[test]
    fn a_stop_that_times_out_reports_it_and_refuses_a_restart() {
        // A wedged maker still holds its port, so `start` must refuse until it is gone.
        // Simulated by a `finished` sender that is still alive.
        let runtime = MakerRuntime::new();
        let (keep_alive, finished) = mpsc::channel();
        lock(&runtime.shared).phase = Phase::Running;
        *lock(&runtime.control) = Some(Control {
            stop_requested: Arc::new(AtomicBool::new(false)),
            thread: None,
            finished,
        });

        assert!(
            !runtime.stop(Duration::from_millis(50), &Logger::silent()),
            "a stop that did not finish must report false"
        );

        // Still live, because the thread it could not join is still out there.
        assert_eq!(runtime.phase(), Phase::Running);
        assert!(runtime.is_live());
        assert!(runtime
            .start(MakerServerConfig::default(), Logger::silent())
            .is_err());

        drop(keep_alive);
    }

    #[test]
    fn a_start_is_refused_while_one_is_already_starting() {
        // Two makers on one port: the second fails to bind, confusingly.
        let runtime = MakerRuntime::new();
        lock(&runtime.shared).phase = Phase::Starting;

        assert!(runtime
            .start(MakerServerConfig::default(), Logger::silent())
            .is_err());
    }

    fn record(phase: &str, recovery: &str, funded: bool) -> SwapRecord {
        SwapRecord {
            id: "abc123".to_string(),
            phase: phase.to_string(),
            recovery: recovery.to_string(),
            amount_sat: 100_000,
            funded,
        }
    }

    #[test]
    fn a_swap_with_a_broadcast_funding_tx_still_recovering_has_funds_at_stake() {
        // Seen on signet: the taker went away mid-swap, funding is on chain, and the maker is
        // waiting on a timelock.
        assert!(record("Recovering", "TimelockWaiting", true).funds_at_stake());
        assert!(record("Recovering", "Monitoring", true).funds_at_stake());
    }

    #[test]
    fn a_swap_that_never_reached_the_chain_has_nothing_at_stake() {
        // No funding broadcast means no contract to recover, so a warning would send the
        // operator looking for money that was never spent.
        assert!(!record("Recovering", "TimelockWaiting", false).funds_at_stake());
    }

    #[test]
    fn a_cleaned_up_or_completed_swap_has_nothing_at_stake() {
        assert!(!record("Recovered", "CleanedUp", true).funds_at_stake());
        assert!(!record("Completed", "NotStarted", true).funds_at_stake());
    }
}
