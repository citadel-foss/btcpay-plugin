//! Owns the taker's wallet: opening it, reporting on it, funding it and withdrawing from it.
//!
//! No swapping yet: a swap must survive a BTCPay restart and needs a quote confirmed before it
//! spends, so it wants a job with progress rather than a command.
//!
//! The wallet is the taker's own, separate from the maker's and from every BTCPay store.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use coinswap::taker::{Taker, TakerInitConfig};
use coinswap::wallet::{AddressType, Balances};

use crate::shared::{describe, lock, Logger, Phase, SendOnDrop};

/// The taker, and the thread that opened it.
///
/// A `Mutex` rather than an `RwLock`: the operations that matter take `&mut self`. Reading the
/// wallet goes through `get_wallet()`, which has its own lock.
struct Shared {
    phase: Phase,
    taker: Option<Arc<Mutex<Taker>>>,
    /// A newly created wallet's seed phrase, held for one display and then dropped.
    ///
    /// Same rule as the maker's: memory only, taken rather than read, gone on restart.
    new_mnemonic: Option<String>,
}

/// Thread bookkeeping, touched only by `start` and `stop`.
struct Control {
    stop_requested: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    finished: Receiver<()>,
}

/// Opens and closes the taker's wallet, and answers questions about it.
pub struct TakerRuntime {
    shared: Arc<Mutex<Shared>>,
    control: Mutex<Option<Control>>,
}

/// A point-in-time view of the taker's wallet, cheap enough for every page load.
pub struct Status {
    pub phase: Phase,
    /// Balances, absent when the wallet is shut or its lock was busy.
    pub balances: Option<Balances>,
    /// True when a lock was held elsewhere, so nothing could be read.
    pub wallet_busy: bool,
    /// Whether an interrupted swap has finished being recovered.
    ///
    /// `None` when undetermined. A half-finished swap leaves funds in timelocked contracts.
    pub recovery_complete: Option<bool>,
}

impl Status {
    fn shut(phase: Phase) -> Self {
        Self {
            phase,
            balances: None,
            wallet_busy: false,
            recovery_complete: None,
        }
    }
}

impl TakerRuntime {
    /// A runtime with no wallet open.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                phase: Phase::Stopped,
                taker: None,
                new_mnemonic: None,
            })),
            control: Mutex::new(None),
        }
    }

    /// What the taker is doing, without querying it.
    pub fn phase(&self) -> Phase {
        lock(&self.shared).phase.clone()
    }

    /// True when the wallet is opening or open.
    pub fn is_live(&self) -> bool {
        matches!(self.phase(), Phase::Starting | Phase::Running)
    }

    /// True when the wallet is open and can be queried.
    pub fn wallet_open(&self) -> bool {
        lock(&self.shared).taker.is_some()
    }

    /// Returns a new wallet's seed phrase the first time it is asked for, and nothing after.
    pub fn take_new_mnemonic(&self) -> Option<String> {
        lock(&self.shared).new_mnemonic.take()
    }

    /// Opens the taker's wallet on its own thread.
    ///
    /// `Taker::init` scans the chain and spawns an offer-sync thread; neither belongs on
    /// BTCPay's startup path.
    pub fn start(&self, config: TakerInitConfig, log: Logger) -> Result<(), String> {
        let mut control = lock(&self.control);
        if self.is_live() {
            return Err("The taker wallet is already open.".to_string());
        }

        let stop_requested = Arc::new(AtomicBool::new(false));
        let (finished_tx, finished_rx) = mpsc::channel();

        {
            let mut shared = lock(&self.shared);
            shared.phase = Phase::Starting;
            shared.taker = None;
        }

        let thread = std::thread::Builder::new()
            .name("coinswap-taker".to_string())
            .spawn({
                let shared = Arc::clone(&self.shared);
                let stop_requested = Arc::clone(&stop_requested);
                move || {
                    let _signal = SendOnDrop(finished_tx);

                    let taker = match Taker::init(config) {
                        Ok(taker) => taker,
                        Err(error) => {
                            let message = describe(error);
                            log.error(&format!("Taker wallet failed to open: {message}"));
                            lock(&shared).phase = Phase::Failed(message);
                            return;
                        }
                    };

                    // Init creates the wallet, so this is the only place the phrase appears.
                    match taker.get_wallet().write() {
                        Ok(mut wallet) => {
                            if let Some(mnemonic) = wallet.take_new_mnemonic() {
                                lock(&shared).new_mnemonic = Some(mnemonic.words());
                                log.info(
                                    "A new taker wallet was created. Its recovery phrase is on \
                                     the taker page and is shown only once. It is a different \
                                     wallet from the maker's, with a different phrase.",
                                );
                            }
                        }
                        Err(_) => log.error(
                            "Could not read the taker wallet after opening it. If this was a new \
                             wallet, its recovery phrase could not be shown.",
                        ),
                    }

                    if stop_requested.load(Ordering::Relaxed) {
                        log.info("Taker wallet closed before it finished opening.");
                        lock(&shared).phase = Phase::Stopped;
                        return;
                    }

                    {
                        let mut shared = lock(&shared);
                        shared.taker = Some(Arc::new(Mutex::new(taker)));
                        shared.phase = Phase::Running;
                    }
                    log.info("Taker wallet is open.");

                    // No server to run, unlike the maker. Recovery of an interrupted swap is
                    // reported on the page rather than forced here, because it moves money.
                }
            })
            .map_err(|error| {
                lock(&self.shared).phase = Phase::Stopped;
                format!("Could not spawn the taker thread: {error}")
            })?;

        *control = Some(Control {
            stop_requested,
            thread: Some(thread),
            finished: finished_rx,
        });

        Ok(())
    }

    /// Closes the wallet, waiting up to `deadline` for the opening thread to finish.
    ///
    /// Returns whether it finished. Never panics: it runs on the shutdown path.
    pub fn stop(&self, deadline: Duration, log: &Logger) -> bool {
        let mut control = lock(&self.control);
        let Some(control) = control.as_mut() else {
            return true;
        };

        control.stop_requested.store(true, Ordering::Relaxed);

        let stopped = match control.finished.recv_timeout(deadline) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(handle) = control.thread.take() {
                    let _ = handle.join();
                }
                true
            }
            Err(RecvTimeoutError::Timeout) => {
                log.error(&format!(
                    "The taker wallet did not close within {}s, so it has been left open.",
                    deadline.as_secs()
                ));
                false
            }
        };

        if stopped {
            let mut shared = lock(&self.shared);
            shared.taker = None;
            if !matches!(shared.phase, Phase::Failed(_)) {
                shared.phase = Phase::Stopped;
            }
        }

        stopped
    }

    /// A point-in-time view for the page.
    ///
    /// Every lock is a `try_` and nothing does network I/O: blocking a render hangs the request.
    pub fn status(&self) -> Status {
        let (phase, taker) = {
            let shared = lock(&self.shared);
            (shared.phase.clone(), shared.taker.clone())
        };

        let Some(handle) = taker else {
            return Status::shut(phase);
        };

        // A local, not a tail expression: tail temporaries outlive what they borrowed from, so
        // naming the result ends the lock borrows before `handle` drops.
        let status = match handle.try_lock() {
            // The taker itself is locked by any swap operation, so this is a `try` too.
            Err(_) => Status {
                phase,
                balances: None,
                wallet_busy: true,
                recovery_complete: None,
            },
            Ok(taker) => {
                let recovery_complete = Some(taker.is_recovery_complete());
                match taker.get_wallet().try_read() {
                    Ok(wallet) => Status {
                        phase,
                        balances: wallet.get_balances().ok(),
                        wallet_busy: false,
                        recovery_complete,
                    },
                    Err(_) => Status {
                        phase,
                        balances: None,
                        wallet_busy: true,
                        recovery_complete,
                    },
                }
            }
        };

        status
    }

    /// Hands out a fresh address for funding the taker's wallet.
    ///
    /// A command, not a page card: it advances the address index and writes to disk.
    pub fn receive_address(&self) -> Result<String, String> {
        self.with_wallet_mut("derive an address", |wallet| {
            wallet
                .get_next_external_address(AddressType::P2WPKH)
                .map(|address| address.to_string())
                .map_err(|error| format!("Could not derive a receive address: {}", describe(error)))
        })
    }

    /// Sends `sats` to `address`, returning the transaction id.
    ///
    /// Checks the address network, so a mainnet address cannot be paid from a signet wallet.
    pub fn withdraw(
        &self,
        address: &str,
        sats: u64,
        fee_rate: Option<f64>,
    ) -> Result<String, String> {
        let address = address.trim().to_string();
        if address.is_empty() {
            return Err("A withdrawal needs a destination address.".to_string());
        }
        if sats == 0 {
            return Err("A withdrawal needs an amount above zero.".to_string());
        }

        self.with_wallet_mut("withdraw", move |wallet| {
            wallet
                .send_to_address(sats, address, fee_rate, None)
                .map(|txid| txid.to_string())
                .map_err(|error| format!("The withdrawal failed: {}", describe(error)))
        })
    }

    /// Runs `body` with the wallet mutably borrowed, or explains why it could not.
    ///
    /// Both locks are `try_`: a blocking command holds the operator's request open with it.
    fn with_wallet_mut<T>(
        &self,
        what: &str,
        body: impl FnOnce(&mut coinswap::wallet::Wallet) -> Result<T, String>,
    ) -> Result<T, String> {
        let handle = lock(&self.shared).taker.clone();
        let Some(handle) = handle else {
            return Err(
                "The taker wallet is not open. Turn the taker on from the settings page."
                    .to_string(),
            );
        };

        let taker = handle
            .try_lock()
            .map_err(|_| format!("The taker is busy, so it cannot {what} right now."))?;

        let mut wallet = taker.get_wallet().try_write().map_err(|_| {
            format!("The taker wallet is busy syncing, so it cannot {what} right now.")
        })?;

        body(&mut wallet)
    }
}

impl Default for TakerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_runtime_has_no_wallet_open() {
        let runtime = TakerRuntime::new();
        assert_eq!(runtime.phase(), Phase::Stopped);
        assert!(!runtime.is_live());
        assert!(!runtime.wallet_open());
        assert!(runtime.take_new_mnemonic().is_none());
    }

    #[test]
    fn closing_a_runtime_that_never_opened_succeeds() {
        let runtime = TakerRuntime::new();
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
        assert!(runtime.stop(Duration::from_secs(1), &Logger::silent()));
    }

    #[test]
    fn a_status_without_a_wallet_still_renders() {
        let runtime = TakerRuntime::new();
        let status = runtime.status();
        assert_eq!(status.phase, Phase::Stopped);
        assert!(status.balances.is_none());
        assert!(!status.wallet_busy);
        assert!(status.recovery_complete.is_none());
    }

    #[test]
    fn wallet_commands_report_that_it_is_shut_rather_than_panicking() {
        // The page is reachable while the taker is off, so commands must survive that.
        let runtime = TakerRuntime::new();
        assert!(runtime.receive_address().is_err());
        assert!(runtime.withdraw("tb1qexample", 1000, None).is_err());
    }

    #[test]
    fn a_withdrawal_needs_a_destination_and_an_amount() {
        // Checked before the wallet is touched, so the message names the real problem.
        let runtime = TakerRuntime::new();
        assert!(runtime
            .withdraw("   ", 1000, None)
            .unwrap_err()
            .contains("address"));
        assert!(runtime
            .withdraw("tb1qexample", 0, None)
            .unwrap_err()
            .contains("amount"));
    }

    #[test]
    fn a_seed_phrase_is_handed_out_once() {
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).new_mnemonic = Some("abandon abandon about".to_string());

        assert!(runtime.take_new_mnemonic().is_some());
        assert!(
            runtime.take_new_mnemonic().is_none(),
            "a reload must not put the phrase back on screen"
        );
    }

    #[test]
    fn a_failure_survives_a_later_close() {
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).phase = Phase::Failed("backend unreachable".to_string());

        runtime.stop(Duration::from_secs(1), &Logger::silent());

        assert_eq!(
            runtime.phase(),
            Phase::Failed("backend unreachable".to_string())
        );
    }
}
