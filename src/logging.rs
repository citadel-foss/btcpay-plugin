//! Forwards openswap's own log output into BTCPay's log.
//!
//! Without an installed logger the `log` crate discards a library's records, and openswap's
//! errors are too thin to diagnose a failure on their own.

use std::sync::{Arc, OnceLock, RwLock};

use btcpay_plugin::{HostServices, LogLevel};

/// Where records go, set when the plugin starts and cleared when it stops.
///
/// Separate from the logger because `log` owns one for the life of the process, while host
/// services come and go with the plugin.
type Sink = RwLock<Option<Arc<dyn HostServices>>>;

static SINK: OnceLock<Sink> = OnceLock::new();

fn sink() -> &'static Sink {
    SINK.get_or_init(|| RwLock::new(None))
}

struct Bridge;

impl log::Log for Bridge {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        // Never held across anything but the host call, so logging cannot block start or stop.
        let Ok(guard) = sink().read() else { return };
        let Some(host) = guard.as_ref() else { return };

        let level = match record.level() {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Trace => LogLevel::Trace,
        };

        // The target prefix separates openswap's records from the plugin's own.
        host.log(level, format!("[{}] {}", record.target(), record.args()));
    }

    fn flush(&self) {}
}

/// Routes openswap's log records to `host` for as long as the plugin is running.
///
/// Safe to call more than once: later calls only swap the destination.
pub fn install(host: Arc<dyn HostServices>) {
    if let Ok(mut guard) = sink().write() {
        *guard = Some(host);
    }

    // A later `Err` means a logger is already in place, which is the desired end state anyway.
    // Logging must never fail a plugin start.
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        if log::set_boxed_logger(Box::new(Bridge)).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }
    });
}

/// Stops forwarding, so no record can reach a host the plugin has already given up.
pub fn detach() {
    if let Ok(mut guard) = sink().write() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_dropped_when_no_host_is_attached() {
        // The logger outlives the plugin, so a record after `stop` must not reach a stale host.
        detach();
        log::info!("this must go nowhere");
    }

    #[test]
    fn installing_twice_does_not_panic() {
        // `set_boxed_logger` refuses a second call; a plugin restart in one BTCPay process does that.
        install(Arc::new(crate::testing::Discard));
        install(Arc::new(crate::testing::Discard));
        detach();
    }
}
