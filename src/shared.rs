//! Pieces the maker and the taker both need.
//!
//! Extracted when the taker arrived rather than duplicated: both roles start a long-lived
//! coinswap object on a thread, both must stop it within a deadline, and both need coinswap's
//! errors turned into something an operator can read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// What a role is doing, as far as the dashboard is concerned.
///
/// Shared by the maker and the taker: both open a wallet, both can fail while doing it, and both
/// want the same four words on a status card.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Not running, and not asked to.
    Stopped,
    /// Connecting to the backend and opening the wallet.
    Starting,
    /// Up: the maker is serving takers, or the taker's wallet is open.
    Running,
    /// It tried and could not. The string is for the operator, not for a machine.
    Failed(String),
}
/// How many times to check that Tor is usable before giving up on it.
///
/// Small on purpose. This covers a Tor that is seconds away from being ready, not a
/// misconfiguration: a wrong control password fails this many times in a row and is then
/// reported, which is the right outcome for something no amount of waiting will fix.
pub(crate) const START_ATTEMPTS: u32 = 4;

/// Multiplied by the attempt number, so waits go 5s, 10s, 15s.
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Waits `total`, returning true if a stop was requested before it elapsed.
///
/// Polled rather than parked on a condvar because the flag it watches is shared with `stop`,
/// which cannot be given a waker without threading one through every caller.
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
/// Turns a coinswap error into something an operator can read.
///
/// `MakerError` implements `Debug` but not `Display`, so the debug form is the only text
/// available. It is not prose, but it names the variant and carries the underlying cause, which
/// is what an operator needs in order to act. Kept in one place so that a `Display` impl
/// appearing upstream is a one-line change here.
pub(crate) fn describe(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}
/// Recovers a poisoned lock rather than propagating the panic.
///
/// A panic while holding one of these locks would otherwise make every later call fail,
/// including `stop`, leaving a maker running that nothing could shut down.
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

    pub(crate) fn info(&self, message: &str) {
        (self.0)(false, message)
    }

    pub(crate) fn error(&self, message: &str) {
        (self.0)(true, message)
    }
}
