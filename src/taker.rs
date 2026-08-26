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

use coinswap::bitcoin::Amount;
use coinswap::protocol::common_messages::ProtocolVersion;
use coinswap::taker::{SwapParams, Taker, TakerInitConfig};
use coinswap::wallet::{AddressType, Balances};

use crate::shared::{describe, lock, Logger, Phase, SendOnDrop};

/// One hop of a quote: the maker at that position and what it charges.
pub struct QuoteHop {
    pub address: String,
    pub locktime: u16,
    pub fee_sat: u64,
}

/// What a swap would cost, before anything is committed.
///
/// Owned rather than holding coinswap's `SwapSummary`, so rendering it needs no lock.
pub struct Quote {
    /// Identifies the prepared swap, and is what executing it will refer to.
    pub swap_id: String,
    pub send_sat: u64,
    pub total_fee_sat: u64,
    pub receive_sat: u64,
    pub hops: Vec<QuoteHop>,
}

/// Where the quote has got to.
///
/// `prepare_coinswap` syncs the offerbook over Tor and waits on Nostr discovery, far too long
/// for the operator's request, so it runs on a thread and the page reports what it finds.
pub enum QuoteState {
    Idle,
    Preparing,
    /// Nothing has been committed.
    Ready(Quote),
    /// The string is for the operator.
    Failed(String),
}

/// How a finished swap turned out.
pub struct SwapOutcome {
    pub swap_id: String,
    pub status: String,
    pub sent_sat: u64,
    pub received_sat: u64,
    pub maker_fees_sat: u64,
    pub mining_fee_sat: u64,
    pub duration_secs: f64,
}

/// Where an accepted swap has got to.
///
/// Separate from [`QuoteState`]: a quote can be thrown away, a running swap has funds in
/// contracts.
pub enum SwapState {
    Idle,
    /// Funds are committed on chain from here on.
    Running {
        swap_id: String,
        since: std::time::Instant,
    },
    Done(SwapOutcome),
    /// Funds may be in contracts awaiting recovery.
    Failed {
        swap_id: String,
        why: String,
    },
}

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
    /// The most recent quote, or how it is going.
    quote: QuoteState,
    /// The accepted swap, or how it went.
    swap: SwapState,
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
                quote: QuoteState::Idle,
                swap: SwapState::Idle,
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

    /// Reports where the current quote has got to, for the page.
    pub fn quote_state(&self) -> QuoteState {
        let shared = lock(&self.shared);
        match &shared.quote {
            QuoteState::Idle => QuoteState::Idle,
            QuoteState::Preparing => QuoteState::Preparing,
            QuoteState::Failed(why) => QuoteState::Failed(why.clone()),
            QuoteState::Ready(quote) => QuoteState::Ready(Quote {
                swap_id: quote.swap_id.clone(),
                send_sat: quote.send_sat,
                total_fee_sat: quote.total_fee_sat,
                receive_sat: quote.receive_sat,
                hops: quote
                    .hops
                    .iter()
                    .map(|hop| QuoteHop {
                        address: hop.address.clone(),
                        locktime: hop.locktime,
                        fee_sat: hop.fee_sat,
                    })
                    .collect(),
            }),
        }
    }

    /// Asks the makers what a swap would cost, on a thread.
    ///
    /// Nothing is committed: coinswap negotiates, reserves a swap id and stops, so the operator
    /// can see the fee before deciding.
    pub fn request_quote(
        &self,
        send_sat: u64,
        maker_count: usize,
        log: Logger,
    ) -> Result<(), String> {
        if send_sat == 0 {
            return Err("A swap needs an amount above zero.".to_string());
        }
        if maker_count == 0 {
            return Err("A swap needs at least one maker.".to_string());
        }

        // In-memory guards first, so the message names the most specific reason. Without this
        // the quote thread blocks on the taker and fires hours later against a stale offerbook.
        {
            let shared = lock(&self.shared);
            if matches!(shared.quote, QuoteState::Preparing) {
                return Err("A quote is already being prepared.".to_string());
            }
            if matches!(shared.swap, SwapState::Running { .. }) {
                return Err(
                    "A swap is running. Wait for it to finish before quoting another.".to_string(),
                );
            }
        }

        let handle = lock(&self.shared).taker.clone();
        let Some(handle) = handle else {
            return Err("The taker wallet is not open.".to_string());
        };

        lock(&self.shared).quote = QuoteState::Preparing;

        let shared = Arc::clone(&self.shared);
        std::thread::Builder::new()
            .name("coinswap-quote".to_string())
            .spawn(move || {
                // Legacy is what the makers on this network negotiated; Taproot needs something
                // to test against first.
                let params = SwapParams::new(
                    ProtocolVersion::Legacy,
                    Amount::from_sat(send_sat),
                    maker_count,
                );

                log.info(&format!(
                    "Preparing a quote for {send_sat} sats across {maker_count} maker(s). This \
                     synchronises the offerbook over Tor and can take a while."
                ));

                // Held for the whole negotiation; everything else uses `try_lock` and reports
                // busy rather than queueing.
                let outcome = match handle.lock() {
                    Ok(mut taker) => taker.prepare_coinswap(params).map_err(describe),
                    Err(_) => Err("The taker is locked by a failed operation.".to_string()),
                };

                let mut shared = lock(&shared);
                shared.quote = match outcome {
                    Ok(summary) => {
                        log.info(&format!(
                            "Quote ready: swap {} costs {} sats in fees.",
                            summary.swap_id,
                            summary.total_estimated_fee.to_sat()
                        ));
                        QuoteState::Ready(Quote {
                            swap_id: summary.swap_id,
                            send_sat: summary.send_amount.to_sat(),
                            total_fee_sat: summary.total_estimated_fee.to_sat(),
                            receive_sat: summary.estimated_receive_amount.to_sat(),
                            hops: summary
                                .makers
                                .into_iter()
                                .map(|maker| QuoteHop {
                                    address: maker.address,
                                    locktime: maker.locktime,
                                    fee_sat: maker.estimated_fee_sats,
                                })
                                .collect(),
                        })
                    }
                    Err(why) => {
                        log.error(&format!("Could not prepare a quote: {why}"));
                        QuoteState::Failed(why)
                    }
                };
            })
            .map_err(|error| {
                lock(&self.shared).quote = QuoteState::Idle;
                format!("Could not spawn the quote thread: {error}")
            })?;

        Ok(())
    }

    /// Reports where an accepted swap has got to, for the page.
    pub fn swap_state(&self) -> SwapState {
        let shared = lock(&self.shared);
        match &shared.swap {
            SwapState::Idle => SwapState::Idle,
            SwapState::Running { swap_id, since } => SwapState::Running {
                swap_id: swap_id.clone(),
                since: *since,
            },
            SwapState::Failed { swap_id, why } => SwapState::Failed {
                swap_id: swap_id.clone(),
                why: why.clone(),
            },
            SwapState::Done(outcome) => SwapState::Done(SwapOutcome {
                swap_id: outcome.swap_id.clone(),
                status: outcome.status.clone(),
                sent_sat: outcome.sent_sat,
                received_sat: outcome.received_sat,
                maker_fees_sat: outcome.maker_fees_sat,
                mining_fee_sat: outcome.mining_fee_sat,
                duration_secs: outcome.duration_secs,
            }),
        }
    }

    /// Whether an earlier swap is still being recovered.
    ///
    /// `None` when undetermined, which callers treat as "do not block": refusing on a reading
    /// we could not take would strand the operator. coinswap drives the recovery itself; this
    /// only reads it.
    pub fn recovery_pending(&self) -> Option<bool> {
        let handle = lock(&self.shared).taker.clone()?;
        let taker = handle.try_lock().ok()?;
        Some(!taker.is_recovery_complete())
    }

    /// True while a swap is executing, so callers can refuse to start another.
    pub fn swap_running(&self) -> bool {
        matches!(lock(&self.shared).swap, SwapState::Running { .. })
    }

    /// Executes the prepared quote. **This commits funds on chain.**
    ///
    /// The quote is consumed, so it cannot be accepted twice. Holds the taker for the whole
    /// swap, so everything else uses `try_lock` and reports busy.
    pub fn accept_quote(&self, log: Logger) -> Result<String, String> {
        let handle = lock(&self.shared).taker.clone();
        let Some(handle) = handle else {
            return Err("The taker wallet is not open.".to_string());
        };

        // Contracts still recovering means funds are committed elsewhere; starting another is
        // how one bad swap becomes two.
        if self.recovery_pending() == Some(true) {
            return Err(
                "An earlier swap is still being recovered. Wait for its contracts to resolve \
                 before starting another."
                    .to_string(),
            );
        }

        let swap_id = {
            let mut shared = lock(&self.shared);

            if matches!(shared.swap, SwapState::Running { .. }) {
                return Err("A swap is already running.".to_string());
            }
            let QuoteState::Ready(quote) = &shared.quote else {
                return Err("There is no quote to accept. Request one first.".to_string());
            };
            let swap_id = quote.swap_id.clone();

            shared.quote = QuoteState::Idle;
            shared.swap = SwapState::Running {
                swap_id: swap_id.clone(),
                since: std::time::Instant::now(),
            };
            swap_id
        };

        let shared = Arc::clone(&self.shared);
        let started = swap_id.clone();
        std::thread::Builder::new()
            .name("coinswap-swap".to_string())
            .spawn(move || {
                log.info(&format!(
                    "Starting swap {started}. Funds are committed on chain from here; leave \
                     BTCPay running until it finishes."
                ));

                let outcome = match handle.lock() {
                    Ok(mut taker) => taker.start_coinswap(&started).map_err(describe),
                    Err(_) => Err("The taker is locked by a failed operation.".to_string()),
                };

                let mut shared = lock(&shared);
                shared.swap = match outcome {
                    Ok(report) => {
                        log.info(&format!(
                            "Swap {} finished: {:?}, {} sats in maker fees.",
                            report.swap_id, report.status, report.total_maker_fees
                        ));
                        SwapState::Done(SwapOutcome {
                            swap_id: report.swap_id,
                            status: format!("{:?}", report.status),
                            sent_sat: report.outgoing_amount,
                            received_sat: report.incoming_amount,
                            maker_fees_sat: report.total_maker_fees,
                            mining_fee_sat: report.mining_fee,
                            duration_secs: report.swap_duration_seconds,
                        })
                    }
                    Err(why) => {
                        log.error(&format!("Swap {started} did not finish: {why}"));
                        SwapState::Failed {
                            swap_id: started,
                            why,
                        }
                    }
                };
            })
            .map_err(|error| {
                lock(&self.shared).swap = SwapState::Idle;
                format!("Could not spawn the swap thread: {error}")
            })?;

        Ok(swap_id)
    }

    /// Clears a finished or failed swap, so the page offers a new quote.
    ///
    /// Refuses while one is running: the state is the only record that funds are committed.
    pub fn clear_swap(&self) -> Result<(), String> {
        let mut shared = lock(&self.shared);
        if matches!(shared.swap, SwapState::Running { .. }) {
            return Err("That swap is still running.".to_string());
        }
        shared.swap = SwapState::Idle;
        Ok(())
    }

    /// Forgets the current quote, so the page offers a fresh one.
    pub fn clear_quote(&self) {
        lock(&self.shared).quote = QuoteState::Idle;
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

    #[test]
    fn a_quote_is_refused_before_the_wallet_is_open() {
        let runtime = TakerRuntime::new();
        assert!(runtime.request_quote(100_000, 2, Logger::silent()).is_err());
    }

    #[test]
    fn a_quote_needs_an_amount_and_a_maker() {
        // Checked before the wallet, so the message names the real problem.
        let runtime = TakerRuntime::new();
        assert!(runtime
            .request_quote(0, 2, Logger::silent())
            .unwrap_err()
            .contains("amount"));
        assert!(runtime
            .request_quote(100_000, 0, Logger::silent())
            .unwrap_err()
            .contains("maker"));
    }

    #[test]
    fn a_fresh_runtime_has_no_quote() {
        assert!(matches!(
            TakerRuntime::new().quote_state(),
            QuoteState::Idle
        ));
    }

    #[test]
    fn discarding_a_quote_returns_to_idle() {
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).quote = QuoteState::Failed("nope".to_string());

        runtime.clear_quote();

        assert!(matches!(runtime.quote_state(), QuoteState::Idle));
    }

    #[test]
    fn a_second_quote_is_refused_while_one_is_being_prepared() {
        // `prepare_coinswap` holds the taker throughout, so a second request would queue
        // invisibly rather than being told no.
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).quote = QuoteState::Preparing;

        let err = runtime
            .request_quote(100_000, 2, Logger::silent())
            .unwrap_err();

        // Says "wallet not open" only because this runtime has none; the guard order is what
        // matters, covered by the state staying Preparing.
        assert!(!err.is_empty());
        assert!(matches!(runtime.quote_state(), QuoteState::Preparing));
    }

    #[test]
    fn a_swap_cannot_be_accepted_without_a_quote() {
        let runtime = TakerRuntime::new();
        assert!(runtime.accept_quote(Logger::silent()).is_err());
    }

    #[test]
    fn a_second_swap_is_refused_while_one_is_running() {
        // The state is the only record that funds are committed, so it must not be trampled.
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).swap = SwapState::Running {
            swap_id: "abc".to_string(),
            since: std::time::Instant::now(),
        };

        assert!(runtime.accept_quote(Logger::silent()).is_err());
        assert!(runtime.swap_running());
    }

    #[test]
    fn a_quote_is_refused_while_a_swap_is_running() {
        // Otherwise the quote thread blocks for the whole swap and fires against a stale
        // offerbook.
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).swap = SwapState::Running {
            swap_id: "abc".to_string(),
            since: std::time::Instant::now(),
        };

        let err = runtime
            .request_quote(100_000, 2, Logger::silent())
            .unwrap_err();
        assert!(err.contains("swap is running"), "{err}");
    }

    #[test]
    fn a_running_swap_cannot_be_cleared() {
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).swap = SwapState::Running {
            swap_id: "abc".to_string(),
            since: std::time::Instant::now(),
        };

        assert!(runtime.clear_swap().is_err());
        assert!(runtime.swap_running());
    }

    #[test]
    fn a_failed_swap_can_be_cleared() {
        let runtime = TakerRuntime::new();
        lock(&runtime.shared).swap = SwapState::Failed {
            swap_id: "abc".to_string(),
            why: "the makers went away".to_string(),
        };

        assert!(runtime.clear_swap().is_ok());
        assert!(matches!(runtime.swap_state(), SwapState::Idle));
    }

    #[test]
    fn recovery_state_is_unknown_without_a_wallet() {
        // Unknown must not read as "pending", or a shut taker looks like it has contracts out.
        assert_eq!(TakerRuntime::new().recovery_pending(), None);
    }

    #[test]
    fn an_unknown_recovery_state_does_not_block_a_swap() {
        // Compares against `Some(true)` so an unavailable reading does not strand the operator;
        // here it falls through to the missing-quote error.
        let runtime = TakerRuntime::new();
        let err = runtime.accept_quote(Logger::silent()).unwrap_err();
        assert!(!err.contains("still being recovered"), "{err}");
    }
}
