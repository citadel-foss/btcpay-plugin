//! Forwards coinswap's own log output into BTCPay's log.
//!
//! coinswap logs through the `log` crate, and a library's records go nowhere unless the binary
//! installs a logger. Nothing did, so everything coinswap had to say about a failure was being
//! discarded, leaving only whatever its `Err` happened to carry. That is thin: several of its
//! errors are a single sentence with the cause logged separately, so a failure would report
//! "Failed to retrieve ephemeral onion service details" while the reason went into a void.
//!
//! Installing this is what makes the maker diagnosable at all.

use std::sync::{Arc, OnceLock, RwLock};

use btcpay_plugin::{HostServices, LogLevel};

/// Where records go, set when the plugin starts and cleared when it stops.
///
/// Separate from the logger itself because `log` takes ownership of a logger for the life of the
/// process and offers no way to replace it, while the host services come and go with the plugin.
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

        // A read lock, and never held across anything but the host call, so a logging thread
        // cannot block the plugin starting or stopping.
        let Ok(guard) = sink().read() else { return };
        let Some(host) = guard.as_ref() else { return };

        let level = match record.level() {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Trace => LogLevel::Trace,
        };

        // Prefixed with the target so it is obvious which of these came from coinswap rather
        // than from the plugin itself.
        host.log(level, format!("[{}] {}", record.target(), record.args()));
    }

    fn flush(&self) {}
}

/// Routes coinswap's log records to `host` for as long as the plugin is running.
///
/// Safe to call more than once: the logger is installed on the first call and later calls only
/// swap the destination.
pub fn install(host: Arc<dyn HostServices>) {
    if let Ok(mut guard) = sink().write() {
        *guard = Some(host);
    }

    // Only the first call can succeed, and a later `Err` means a logger is already in place,
    // which is the desired end state either way. Nothing here should fail a plugin start:
    // logging not working is worth far less than the maker running.
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
        // The logger outlives the plugin, so a record arriving after `stop` must not panic or
        // reach a stale host.
        detach();
        log::info!("this must go nowhere");
    }

    #[test]
    fn installing_twice_does_not_panic() {
        // `log::set_boxed_logger` refuses a second call, and a plugin restart within one BTCPay
        // process would make exactly that happen.
        install(Arc::new(Discard));
        install(Arc::new(Discard));
        detach();
    }

    /// A `HostServices` that does nothing, for the tests above.
    struct Discard;

    impl HostServices for Discard {
        fn get_setting(&self, _key: String) -> Option<String> {
            None
        }
        fn set_setting(
            &self,
            _key: String,
            _value: String,
        ) -> Result<(), btcpay_plugin::HostError> {
            Ok(())
        }
        fn store_get(&self, _key: String) -> Option<Vec<u8>> {
            None
        }
        fn store_put(&self, _key: String, _value: Vec<u8>) -> Result<(), btcpay_plugin::HostError> {
            Ok(())
        }
        fn store_delete(&self, _key: String) -> Result<(), btcpay_plugin::HostError> {
            Ok(())
        }
        fn data_dir(&self) -> String {
            String::new()
        }
        fn log(&self, _level: LogLevel, _message: String) {}
        fn emit_notification(
            &self,
            _notification: btcpay_plugin::Notification,
        ) -> Result<(), btcpay_plugin::HostError> {
            Ok(())
        }
        fn send_webhook(
            &self,
            _webhook: btcpay_plugin::WebhookRequest,
        ) -> Result<(), btcpay_plugin::HostError> {
            Ok(())
        }
    }
}
