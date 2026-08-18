//! Owns the maker's lifecycle: starting it, stopping it inside a deadline, and reporting what
//! it is doing.
//!
//! Two properties of coinswap drive the design here.
//!
//! **`MakerServer::init` talks to a Bitcoin node and possibly Tor**, so it can take tens of
//! seconds or hang on a misconfigured backend. `Plugin::start` runs on BTCPay's startup path,
//! so init happens on the maker's own thread and `start` returns immediately. The cost is that
//! a configuration error surfaces on the dashboard rather than as a start failure, which is the
//! better trade: a plugin that refuses to load takes its own settings page down with it,
//! leaving the operator no way to fix the setting that broke it.
//!
//! **`start_server` blocks for the maker's whole life**, so it is the thread's body rather than
//! something called and returned from.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use coinswap::maker::{start_server, MakerServer, MakerServerConfig};
use coinswap::utill::check_tor_status;
use coinswap::wallet::{AddressType, Balances};

/// What the maker is doing, as far as the dashboard is concerned.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Not running, and not asked to.
    Stopped,
    /// Connecting to the backend and opening the wallet.
    Starting,
    /// Serving takers.
    Running,
    /// It tried and could not. The string is for the operator, not for a machine.
    Failed(String),
}

/// State both the maker thread and the plugin touch.
///
/// One lock rather than a lock per field, so a reader cannot observe a phase that disagrees
/// with the server handle beside it.
struct Shared {
    phase: Phase,
    /// `None` until init finishes, which is why a stop arriving mid-init needs `stop_requested`
    /// as well as this.
    maker: Option<Arc<MakerServer>>,
    /// A newly created wallet's seed phrase, held for one display and then dropped.
    ///
    /// coinswap gives `SecretMnemonic` no `Debug` and no `Display` on purpose, and its docs say
    /// not to log or persist it. So this is memory only: it never reaches the settings store,
    /// the log, or disk. If BTCPay restarts before the operator reads it, it is gone, which is
    /// the correct direction to fail in for a seed phrase.
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
pub struct Runtime {
    shared: Arc<Mutex<Shared>>,
    control: Mutex<Option<Control>>,
}

/// How many times to check that Tor is usable before giving up on it.
///
/// Small on purpose. This covers a Tor that is seconds away from being ready, not a
/// misconfiguration: a wrong control password fails this many times in a row and is then
/// reported, which is the right outcome for something no amount of waiting will fix.
const START_ATTEMPTS: u32 = 4;

/// Multiplied by the attempt number, so waits go 5s, 10s, 15s.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Waits `total`, returning true if a stop was requested before it elapsed.
///
/// Polled rather than parked on a condvar because the flag it watches is shared with `stop`,
/// which cannot be given a waker without threading one through every caller.
fn sleep_unless_stopped(total: Duration, stop_requested: &AtomicBool) -> bool {
    const TICK: Duration = Duration::from_millis(200);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop_requested.load(Ordering::Relaxed) {
            return true;
        }
        let step = TICK.min(total - slept);
        std::thread::sleep(step);
        slept += step;
    }
    stop_requested.load(Ordering::Relaxed)
}

/// Turns a coinswap error into something an operator can read.
///
/// `MakerError` implements `Debug` but not `Display`, so the debug form is the only text
/// available. It is not prose, but it names the variant and carries the underlying cause, which
/// is what an operator needs in order to act. Kept in one place so that a `Display` impl
/// appearing upstream is a one-line change here.
fn describe(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

/// Recovers a poisoned lock rather than propagating the panic.
///
/// A panic while holding one of these locks would otherwise make every later call fail,
/// including `stop`, leaving a maker running that nothing could shut down.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Runtime {
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
    /// Taking rather than reading is the point: the phrase is shown once, and a page reload must
    /// not put it back on screen.
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
                    // Sends `finished` however this body leaves, including on a panic, so a
                    // stop is never left waiting out the full deadline for a thread that is
                    // already gone.
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

                    // A brand new wallet hands back its seed phrase exactly once, and only
                    // here, because init is what creates the wallet.
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

                    // A stop that arrived during init had no server to signal, so honour it
                    // here rather than starting one we were already told to shut down.
                    if stop_requested.load(Ordering::Relaxed) {
                        log.info("Maker stopped before it finished starting.");
                        lock(&shared).phase = Phase::Stopped;
                        return;
                    }

                    // Only the Tor check is retried, and `start_server` is then called exactly
                    // once. That split matters: `start_server` is not idempotent -- it spawns a
                    // watchtower and other background work each time -- so an earlier version
                    // that retried it left several watchtowers running against one wallet. The
                    // retryable condition is "is Tor ready", which is a question, not an action.
                    //
                    // Worth waiting for because Tor cannot come up before BTCPay in the dev
                    // stack: it borrows BTCPay's network namespace, so it necessarily starts
                    // after, and on a real server the two come up independently.
                    let mut tor = Err("not checked".to_string());
                    for attempt in 1..=START_ATTEMPTS {
                        if stop_requested.load(Ordering::Relaxed) {
                            lock(&shared).phase = Phase::Stopped;
                            return;
                        }

                        // Checked at all because coinswap's own failure here is indistinguishable
                        // prose: whether Tor is absent, rejected the password or refused the key,
                        // `start_server` reports "Failed to retrieve ephemeral onion service
                        // details" and nothing more. This separates them, and a wrong control
                        // password is much the likeliest, the setting defaulting to empty while
                        // Tor requires one.
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

                    // `start_server` only returns on failure: success means it blocks here for the
                    // maker's whole life, so setting Running just before the call is the closest
                    // thing to a "started" signal coinswap offers.
                    let outcome = match tor {
                        Ok(()) => {
                            lock(&shared).phase = Phase::Running;
                            log.info("Maker is running.");
                            start_server(Arc::clone(&maker)).map_err(describe)
                        }
                        Err(message) => Err(message),
                    };

                    // A requested shutdown usually surfaces as an error out of the accept loop,
                    // so it is only a failure if nobody asked to stop.
                    let asked_to_stop =
                        stop_requested.load(Ordering::Relaxed) || maker.is_shutdown();
                    let mut shared = lock(&shared);
                    shared.phase = match outcome {
                        Err(message) if !asked_to_stop => {
                            log.error(&format!("Maker stopped serving: {message}"));
                            // The handle is deliberately kept here rather than cleared. Init
                            // succeeded, so the wallet behind it is open and valid, and it is the
                            // only way to read a balance or hand out a receive address. A maker
                            // that could not start is precisely when an operator needs both,
                            // because the usual reason is a wallet with nothing in it.
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
    /// Returns whether the thread actually finished. `false` means it is still running and has
    /// been left detached: blocking BTCPay's shutdown on a wedged maker is worse than leaking a
    /// thread in a process that is on its way out. Never panics, because it runs on the
    /// shutdown path.
    pub fn stop(&self, deadline: Duration, log: &Logger) -> bool {
        let mut control = lock(&self.control);
        let Some(control) = control.as_mut() else {
            return true;
        };

        control.stop_requested.store(true, Ordering::Relaxed);

        // Clone the handle out and release the lock before waiting, so a dashboard render in
        // another thread is not blocked for the whole deadline.
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

        // Deliberately no phase reset when it did not stop. The detached thread still holds
        // `shared` and will set the phase itself when `start_server` finally returns, so writing
        // `Stopped` here would be overwritten anyway, and in the meantime it would be a lie: the
        // maker is still up. Leaving the phase as it is also means `is_live` stays true, so
        // `start` refuses until the old thread is genuinely gone. That is what keeps a second
        // maker from being spawned onto the same port, and it clears itself with no
        // bookkeeping.
        stopped
    }

    /// A point-in-time view of the maker for the dashboard.
    ///
    /// Every query here is allowed to fail into `None`. The maker holds its wallet across chain
    /// I/O, so a page load can arrive while the lock is held, and a dashboard that renders
    /// "unavailable" for one field beats a page that errors out entirely.
    pub fn status(&self) -> Status {
        let (phase, maker) = {
            let shared = lock(&self.shared);
            (shared.phase.clone(), shared.maker.clone())
        };

        let Some(maker) = maker else {
            return Status::without_maker(phase);
        };

        // `try_read`, never `read`. coinswap's startup takes the wallet's *write* lock in a loop
        // while it waits for the fidelity bond to be funded, holding it across a chain sync each
        // time round. A blocking read here made the dashboard hang for as long as that went on,
        // which is indefinitely on an unfunded maker -- the one state in which an operator most
        // needs the page. Rendering "busy" is the only acceptable behaviour.
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

        // Deliberately no `check_swap_liquidity` here: it makes RPC calls, and a page render must
        // not do network I/O. It was the second reason this page could hang.
        Status {
            phase,
            balances,
            // Cheap: a mutex around a collection, no I/O.
            ongoing_swaps: maker.has_ongoing_swaps().ok(),
            bonds,
            wallet_busy,
            port: Some(maker.config.network_port),
        }
    }

    /// True when the wallet is open and can be queried, which outlasts a failed start.
    pub fn wallet_open(&self) -> bool {
        lock(&self.shared).maker.is_some()
    }

    /// Hands out a fresh address for funding the maker's wallet.
    ///
    /// A command rather than something on the dashboard because it advances the wallet's address
    /// index and writes to disk, which a page render must not do on every load.
    pub fn receive_address(&self) -> Result<String, String> {
        let maker = lock(&self.shared).maker.clone();
        let Some(maker) = maker else {
            return Err(
                "The maker's wallet is not open yet. Start the maker, and if it fails, its \
                 wallet stays open so this will work."
                    .to_string(),
            );
        };

        // `try_write` for the same reason `status` uses `try_read`: coinswap's funding wait holds
        // this lock across a chain sync, and a command that blocks on it holds the operator's
        // request open too.
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

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// A point-in-time view of the maker, cheap enough to build on every page load.
///
/// A snapshot rather than a borrow of the live server: rendering must not hold the wallet lock
/// while the maker wants it.
pub struct Status {
    /// What the maker is doing.
    pub phase: Phase,
    /// Wallet balances, absent when the maker is down or the wallet lock was busy.
    pub balances: Option<Balances>,
    /// Whether a swap is in flight. `None` when it could not be determined.
    pub ongoing_swaps: Option<bool>,
    /// Fidelity bonds held, as (amount in sats, locktime).
    pub bonds: Vec<(u64, String)>,
    /// True when the wallet was locked, so balances and bonds could not be read.
    ///
    /// Not an error. coinswap holds the wallet across chain syncs, so this is normal for seconds
    /// at a time, and continuous while it waits to be funded.
    pub wallet_busy: bool,
    /// The port takers reach this maker on, when it is up.
    pub port: Option<u16>,
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
        }
    }
}

/// Signals on drop, so a panicking thread still reports that it is done.
struct SendOnDrop(mpsc::Sender<()>);

impl Drop for SendOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Logging the runtime can do without knowing about the host.
///
/// The maker thread outlives the call that spawned it, so it cannot borrow a `HostServices`, and
/// the runtime is exercised in tests that have none.
#[derive(Clone)]
pub struct Logger(Arc<LogSink>);

/// The callable a [`Logger`] wraps: (is_error, message).
type LogSink = dyn Fn(bool, &str) + Send + Sync;

impl Logger {
    /// Wraps a sink taking (is_error, message).
    pub fn new(sink: impl Fn(bool, &str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }

    /// Discards everything. For tests.
    pub fn silent() -> Self {
        Self::new(|_, _| {})
    }

    fn info(&self, message: &str) {
        (self.0)(false, message)
    }

    fn error(&self, message: &str) {
        (self.0)(true, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_runtime_is_stopped_and_has_no_seed_phrase() {
        let runtime = Runtime::new();
        assert_eq!(runtime.phase(), Phase::Stopped);
        assert!(!runtime.is_live());
        assert!(runtime.take_new_mnemonic().is_none());
    }

    #[test]
    fn stopping_a_runtime_that_never_started_succeeds() {
        // `stop` runs on BTCPay's shutdown path, where throwing is not an option.
        let runtime = Runtime::new();
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
    }

    #[test]
    fn stopping_twice_succeeds() {
        let runtime = Runtime::new();
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
    }

    #[test]
    fn commands_report_that_the_wallet_is_shut_rather_than_panicking() {
        // An operator can reach the dashboard while the maker is stopped, so every command has
        // to survive being pressed then.
        let runtime = Runtime::new();
        assert!(runtime.receive_address().is_err());
        assert!(runtime.drain_idle_swaps(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn a_status_without_a_maker_still_renders() {
        let runtime = Runtime::new();
        let status = runtime.status();
        assert_eq!(status.phase, Phase::Stopped);
        assert!(status.balances.is_none());
        assert!(status.port.is_none());
        assert!(status.bonds.is_empty());
    }

    #[test]
    fn a_seed_phrase_is_handed_out_once() {
        let runtime = Runtime::new();
        lock(&runtime.shared).new_mnemonic = Some("abandon abandon about".to_string());

        assert!(runtime.take_new_mnemonic().is_some());
        assert!(
            runtime.take_new_mnemonic().is_none(),
            "a reload must not put the phrase back on screen"
        );
    }

    #[test]
    fn a_poisoned_lock_does_not_wedge_the_runtime() {
        // If a panic while holding the lock made every later call fail, `stop` would fail too
        // and a running maker would become unstoppable.
        let runtime = Runtime::new();
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
        // The dashboard has to keep explaining why the maker went away; overwriting the reason
        // with a bare "stopped" is how an operator loses the only clue they had.
        let runtime = Runtime::new();
        lock(&runtime.shared).phase = Phase::Failed("backend unreachable".to_string());

        runtime.stop(Duration::from_secs(1), &Logger::silent());

        assert_eq!(
            runtime.phase(),
            Phase::Failed("backend unreachable".to_string())
        );
    }

    #[test]
    fn a_stop_that_times_out_reports_it_and_refuses_a_restart() {
        // The maker that would not stop is still holding its port and its wallet. Starting a
        // second one onto the same port would fail confusingly, so `start` must refuse until the
        // first is genuinely gone. Simulated by a `finished` channel whose sender is still alive,
        // which is exactly the state a wedged thread leaves behind.
        let runtime = Runtime::new();
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
        // Two makers on one port, one of which would fail to bind and report a confusing error.
        let runtime = Runtime::new();
        lock(&runtime.shared).phase = Phase::Starting;

        assert!(runtime
            .start(MakerServerConfig::default(), Logger::silent())
            .is_err());
    }
}
