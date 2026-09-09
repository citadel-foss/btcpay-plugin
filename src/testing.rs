//! Test doubles shared by the modules' own test suites.

use btcpay_plugin::prelude::{HostError, HostServices, LogLevel, Notification, WebhookRequest};

/// A `HostServices` that accepts everything and remembers nothing.
pub struct Discard;

impl HostServices for Discard {
    fn get_setting(&self, _key: String) -> Option<String> {
        None
    }
    fn set_setting(&self, _key: String, _value: String) -> Result<(), HostError> {
        Ok(())
    }
    fn store_get(&self, _key: String) -> Option<Vec<u8>> {
        None
    }
    fn store_put(&self, _key: String, _value: Vec<u8>) -> Result<(), HostError> {
        Ok(())
    }
    fn store_delete(&self, _key: String) -> Result<(), HostError> {
        Ok(())
    }
    fn data_dir(&self) -> String {
        String::new()
    }
    fn log(&self, _level: LogLevel, _message: String) {}
    fn emit_notification(&self, _notification: Notification) -> Result<(), HostError> {
        Ok(())
    }
    fn send_webhook(&self, _webhook: WebhookRequest) -> Result<(), HostError> {
        Ok(())
    }
}
