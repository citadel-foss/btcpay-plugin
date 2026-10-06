//! Pieces the maker and the taker both need.

use openswap::wallet::{UTXOSpendInfo, Wallet};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// What a role is doing, as far as the dashboard is concerned.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Not running, and not asked to.
    Stopped,
    /// Connecting to the backend and opening the wallet.
    Starting,
    /// Started, and doing its job.
    Running,
    /// The string is for the operator, not for a machine.
    Failed(String),
}
/// How many times to check that Tor is usable before giving up on it.
///
/// Small on purpose: this covers a Tor seconds from ready, not a misconfiguration.
pub(crate) const START_ATTEMPTS: u32 = 4;

/// Multiplied by the attempt number, so waits go 5s, 10s, 15s.
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Waits `total`, returning true if a stop was requested before it elapsed.
///
/// Polled rather than parked on a condvar: the flag is shared with `stop`, which has no waker.
pub(crate) fn sleep_unless_stopped(total: Duration, stop_requested: &AtomicBool) -> bool {
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
/// Seconds since the Unix epoch, the unit openswap timestamps its records in.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Turns a openswap error into something an operator can read.
///
/// `MakerError` has no `Display`, so the debug form is the only text available. In one place so
/// an upstream `Display` is a one-line change.
pub(crate) fn describe(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}
/// Recovers a poisoned lock rather than propagating the panic.
///
/// Otherwise a panic under the lock makes `stop` fail too, leaving an unstoppable maker.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Signals on drop, so a panicking thread still reports that it is done.
pub(crate) struct SendOnDrop(pub(crate) mpsc::Sender<()>);

impl Drop for SendOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Logging the runtime can do without knowing about the host.
///
/// The maker thread outlives the call that spawned it, so it cannot borrow a `HostServices`.
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

    pub(crate) fn info(&self, message: &str) {
        (self.0)(false, message)
    }

    pub(crate) fn error(&self, message: &str) {
        (self.0)(true, message)
    }
}

/// One unspent output, in the shape a page prints it.
///
/// Flattened rather than handing openswap's own types to a page: printing a row should not
/// require knowing what a `UTXOSpendInfo` is.
pub struct Coin {
    /// What the coin is worth, in satoshis.
    pub amount_sat: u64,
    /// What the coin is doing: spendable, mid-swap, held in a contract, or bonded.
    pub kind: &'static str,
    /// Confirmations behind it. Zero means it is still in the mempool.
    pub confirmations: u32,
    /// The address holding it, or `-` when the entry carries none.
    pub address: String,
    /// `txid:vout`, the coin's identity on chain.
    pub outpoint: String,
}

/// Every unspent output, flattened for the page and largest first.
pub fn coins_of(wallet: &Wallet) -> Vec<Coin> {
    let mut coins: Vec<Coin> = wallet
        .list_all_utxo_spend_info()
        .into_iter()
        .map(|(utxo, info)| Coin {
            amount_sat: utxo.amount.to_sat(),
            kind: kind_of(&info),
            confirmations: utxo.confirmations,
            // A watch-only entry can lack an address; the outpoint still identifies the coin.
            address: utxo.address.map_or_else(
                || "-".to_string(),
                |address| address.assume_checked().to_string(),
            ),
            outpoint: format!("{}:{}", utxo.txid, utxo.vout),
        })
        .collect();
    coins.sort_by_key(|coin| std::cmp::Reverse(coin.amount_sat));
    coins
}

/// What a coin is doing, in words rather than openswap's type names.
fn kind_of(info: &UTXOSpendInfo) -> &'static str {
    match info {
        UTXOSpendInfo::SeedCoin { .. } => "Spendable",
        UTXOSpendInfo::IncomingSwapCoin { .. } => "From a swap",
        UTXOSpendInfo::OutgoingSwapCoin { .. } => "Sent in a swap",
        UTXOSpendInfo::TimelockContract { .. } => "Timelock contract",
        UTXOSpendInfo::HashlockContract { .. } => "Hashlock contract",
        UTXOSpendInfo::FidelityBondCoin { .. } => "Fidelity bond",
        UTXOSpendInfo::SweptCoin { .. } => "Swept",
    }
}
