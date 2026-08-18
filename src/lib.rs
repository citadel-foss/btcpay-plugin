//! A coinswap maker, run and supervised from inside BTCPay Server.
//!
//! A maker is a long-lived service that advertises itself to takers, locks funds into a fidelity
//! bond and earns fees on swaps. Running one normally means `makerd`, a config file and a
//! terminal. This plugin puts it where an operator already is: settings in BTCPay's UI, a
//! dashboard showing what the maker is doing, and buttons for the few operations that are not a
//! setting.
//!
//! # Scope
//!
//! The **maker** side only. A taker spends money to gain privacy on demand, which is a wallet
//! operation and a different plugin.
//!
//! The maker keeps its own wallet under the plugin's data directory. It is not a BTCPay store
//! wallet, BTCPay never sees it, and money in it is not money in a store.
//!
//! # Layout
//!
//! - [`settings`] is what the operator fills in, and the one place it becomes a
//!   `MakerServerConfig`.
//! - [`maker`] owns the maker's thread, because `start_server` blocks and `init` does chain I/O
//!   that must not run on BTCPay's startup path.
//! - [`shared`] is what any role needs: the phase, the logger, the lock helper.
//! - [`logging`] routes coinswap's own log output into BTCPay's, without which a failure inside
//!   coinswap reports only whatever its error happened to carry.
//! - This module is the glue: pages, commands, and reacting to a settings change.

#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod logging;
pub mod maker;
pub mod settings;
pub mod shared;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btcpay_plugin::prelude::*;

use maker::{MakerRuntime, Status};
use settings::Settings;
use shared::{lock, Logger, Phase};

/// How long a drain command waits for an idle swap to wind up.
///
/// Short, because the command runs inside the operator's request. See the note on commands in
/// the milestone: long-running work in a request is a known rough edge.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a stop waits for the maker before giving up on it.
///
/// A maker mid-swap has contracts to watch, so it is worth waiting for. Past this, BTCPay's own
/// shutdown matters more.
const STOP_DEADLINE: Duration = Duration::from_secs(30);

/// The plugin.
#[derive(Default)]
pub struct CoinswapPlugin {
    maker: MakerRuntime,
    /// Settings as last loaded or saved, so rendering a page does not re-read storage field by
    /// field across the FFI boundary.
    settings: Mutex<Option<Settings>>,
    /// Kept from `start` so a command can log and find the data directory.
    host: Mutex<Option<Arc<dyn HostServices>>>,
}

impl CoinswapPlugin {
    fn host(&self) -> Option<Arc<dyn HostServices>> {
        lock(&self.host).clone()
    }

    fn settings(&self) -> Settings {
        lock(&self.settings).clone().unwrap_or_default()
    }

    /// A logger that forwards into BTCPay's log, usable from the maker's own thread.
    fn logger(host: Arc<dyn HostServices>) -> Logger {
        Logger::new(move |is_error, message| {
            let level = if is_error {
                LogLevel::Error
            } else {
                LogLevel::Info
            };
            host.log(level, message.to_string());
        })
    }

    /// Where the maker keeps its wallet.
    ///
    /// A subdirectory rather than the data directory itself, so the wallet does not sit
    /// alongside whatever else the plugin writes later.
    fn maker_dir(host: &dyn HostServices) -> PathBuf {
        PathBuf::from(host.data_dir()).join("maker")
    }

    /// Brings the maker into line with the settings: running when enabled, stopped when not.
    ///
    /// Returns a message for the operator, because this runs in response to a save or a button
    /// press and either way somebody is waiting to hear what happened.
    fn reconcile(&self, settings: &Settings) -> Result<String, String> {
        let Some(host) = self.host() else {
            return Err("The plugin has not finished starting yet.".to_string());
        };
        let logger = Self::logger(Arc::clone(&host));

        if !settings.enabled {
            if self.maker.is_live() {
                return Ok(if self.maker.stop(STOP_DEADLINE, &logger) {
                    "Maker stopped.".to_string()
                } else {
                    "Maker was asked to stop but did not finish in time.".to_string()
                });
            }
            return Ok("Maker is off.".to_string());
        }

        settings.check()?;

        // Restart rather than reconfigure. `init` consumes the config and the running server
        // owns it, so there is no way to change a fee or a port underneath a live maker.
        if self.maker.is_live() {
            self.maker.stop(STOP_DEADLINE, &logger);
        }

        let dir = Self::maker_dir(host.as_ref());
        std::fs::create_dir_all(&dir).map_err(|error| {
            format!(
                "Could not create the maker directory at {}: {error}",
                dir.display()
            )
        })?;

        self.maker.start(settings.to_maker_config(dir), logger)?;

        Ok("Maker is starting. Reload the dashboard to see it come up.".to_string())
    }

    /// Builds the dashboard.
    fn dashboard(&self) -> Document {
        let settings = self.settings();
        let status = self.maker.status();

        let mut page = Document::new("Coinswap maker");

        // A new wallet's phrase, first and once. An operator who misses it cannot recover the
        // wallet, so it outranks everything else on the page.
        if let Some(words) = self.maker.take_new_mnemonic() {
            page = page
                .alert(
                    AlertLevel::Warning,
                    "A new maker wallet was created. Write down the recovery phrase below now. \
                     It is shown once, it is not stored anywhere this page can read again, and \
                     without it the funds in this wallet cannot be recovered.",
                )
                .text(words);
        }

        let (level, text) = Self::phase_alert(&status, &settings);
        page = page.alert(level, text);

        let mut stats = Stats::new()
            .card("Status", Self::phase_label(&status.phase))
            .card("Network", format!("{:?}", settings.chain))
            .card(
                "Port",
                status
                    .port
                    .map_or_else(|| "-".to_string(), |port| port.to_string()),
            );

        if status.wallet_busy {
            stats = stats
                .card("Balances", "Busy")
                .detail("The wallet is locked by the maker; reload once it is done.");
        }

        if let Some(balances) = &status.balances {
            stats = stats
                .card("Spendable", Self::sats(balances.spendable.to_sat()))
                .detail("The maker's own wallet, not a BTCPay store wallet.")
                .card("In swaps", Self::sats(balances.swap.to_sat()))
                .card("In bonds", Self::sats(balances.fidelity.to_sat()))
                .detail("Locked until the bond timelock expires.")
                .card("In contracts", Self::sats(balances.contract.to_sat()));
        }

        stats = stats.card(
            "Swap in progress",
            match status.ongoing_swaps {
                Some(true) => "Yes",
                Some(false) => "No",
                None => "Unknown",
            },
        );

        page = page.stats(stats);

        let mut bonds = Table::new(["Amount", "Locked until"])
            .title("Fidelity bonds")
            .empty_message(
                "No fidelity bond yet. The maker creates one while starting, which needs a \
                 funded wallet; takers will not choose a maker without one.",
            );
        for (amount, lock_time) in &status.bonds {
            bonds = bonds.row([Self::sats(*amount), lock_time.clone()]);
        }
        page = page.table(bonds);

        if status.wallet_busy {
            page = page.alert(
                AlertLevel::Info,
                "The maker is using its wallet right now, so balances and bonds are not shown. \
                 That is normal during a chain sync, and continuous while the maker is waiting \
                 for its fidelity bond to be funded. BTCPay's log shows what it is doing, \
                 including the address it wants funded.",
            );
        }

        page.actions(self.commands(&status, &settings))
    }

    /// The buttons, which depend on what the maker is currently doing.
    ///
    /// Offering a button that cannot work is worse than not offering it: an operator learns
    /// nothing from a page that lets them press "Stop" on a maker that is already stopped.
    fn commands(&self, status: &Status, settings: &Settings) -> Actions {
        let mut actions = Actions::new().title("Operations");

        if self.maker.is_live() {
            actions = actions.button(Button::new("stop", "Stop the maker").destructive(
                if status.ongoing_swaps == Some(true) {
                    "A swap is in progress. Stopping now risks it timing out and falling back to \
                     a timelocked refund. Stop anyway?"
                } else {
                    "Stop the maker? Takers will no longer be able to reach it."
                },
            ));

            actions = actions.button(Button::new("drain", "Drain idle swaps").confirm(
                "Clear out swaps that have gone idle? A counterparty that comes back will find \
                 its swap gone and fall back to its timelocked refund.",
            ));
        } else if settings.enabled {
            actions = actions.button(Button::new("start", "Start the maker").primary());
        }

        // Offered whenever the wallet is open, which includes a maker that failed to start. That
        // is the case that matters: the maker cannot start without a funded wallet, so refusing
        // to hand out an address until it is running would be a deadlock.
        if self.maker.wallet_open() {
            actions = actions.button(Button::new("address", "Show a funding address"));
        }

        // No fidelity bond command. coinswap creates the bond inside `start_server`, committed to
        // the maker's onion hostname, and that hostname comes from a private method this plugin
        // cannot call. A button here could only commit a bond to some other address, and a bond
        // is unspendable until its timelock expires -- so it would lock funds into something no
        // taker would ever use. Renewal happens on the next start.

        actions
    }

    /// The banner at the top of the dashboard: what went wrong, or what to do next.
    fn phase_alert(status: &Status, settings: &Settings) -> (AlertLevel, String) {
        match &status.phase {
            Phase::Failed(reason) => (
                AlertLevel::Danger,
                format!(
                    "The maker is not running: {reason}. Check the settings page, then start it \
                     again."
                ),
            ),
            Phase::Starting => (
                AlertLevel::Info,
                "The maker is starting. Connecting to the chain backend takes a while, and \
                 longer over Tor."
                    .to_string(),
            ),
            Phase::Running if status.bonds.is_empty() => (
                AlertLevel::Warning,
                "The maker is running but has no fidelity bond, so takers will not choose it."
                    .to_string(),
            ),
            Phase::Running => (
                AlertLevel::Success,
                "The maker is running and advertising itself to takers.".to_string(),
            ),
            Phase::Stopped if !settings.enabled => (
                AlertLevel::Info,
                "The maker is off. Fill in the settings page, turn on \"Run the maker\", and \
                 save."
                    .to_string(),
            ),
            Phase::Stopped => (
                AlertLevel::Warning,
                "The maker is enabled but not running. Start it below.".to_string(),
            ),
        }
    }

    fn phase_label(phase: &Phase) -> &'static str {
        match phase {
            Phase::Stopped => "Stopped",
            Phase::Starting => "Starting",
            Phase::Running => "Running",
            Phase::Failed(_) => "Failed",
        }
    }

    /// Groups digits, because a bond amount in sats is long enough to misread by a factor of ten.
    fn sats(sats: u64) -> String {
        let digits = sats.to_string();
        let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        format!("{grouped} sats")
    }

    /// Runs a button press.
    ///
    /// Every arm reports something, because a button that appears to do nothing is
    /// indistinguishable from one that failed silently.
    fn run_command(&self, command: &str) -> Vec<PluginAction> {
        let settings = self.settings();

        let outcome = match command {
            "start" => {
                if settings.enabled {
                    self.reconcile(&settings)
                } else {
                    Err("Turn on \"Run the maker\" on the settings page first.".to_string())
                }
            }
            // Deliberately does not clear `enabled`. This stops the maker now and leaves it
            // startable again from this page; a BTCPay restart brings it back, which is what
            // "enabled" ought to mean.
            "stop" => match self.host() {
                Some(host) => {
                    if self.maker.stop(STOP_DEADLINE, &Self::logger(host)) {
                        Ok("Maker stopped.".to_string())
                    } else {
                        Err(format!(
                            "The maker did not stop within {}s and has been left running in the \
                             background. Restarting BTCPay will clear it.",
                            STOP_DEADLINE.as_secs()
                        ))
                    }
                }
                None => Err("The plugin has not finished starting yet.".to_string()),
            },
            "address" => self.maker.receive_address().map(|address| {
                format!(
                    "Send funds to {address} -- this is the maker's own wallet, not a store \
                     wallet. The address advances each time, so take a fresh one per deposit."
                )
            }),
            "drain" => self.maker.drain_idle_swaps(DRAIN_TIMEOUT).map(|count| {
                if count == 0 {
                    "No idle swaps needed draining.".to_string()
                } else {
                    format!("Drained {count} idle swap(s).")
                }
            }),
            other => Err(format!("Unknown command: {other}")),
        };

        match outcome {
            Ok(text) => vec![PluginAction::ShowMessage {
                level: MessageLevel::Success,
                text,
            }],
            // Logged as well as shown: a failure that moved money part of the way needs to
            // survive the operator closing the page.
            Err(text) => vec![
                PluginAction::Log {
                    level: LogLevel::Error,
                    message: format!("Command '{command}' failed: {text}"),
                },
                PluginAction::ShowMessage {
                    level: MessageLevel::Error,
                    text,
                },
            ],
        }
    }
}

#[btcpay_plugin::plugin(identifier = "BTCPayServer.Plugins.Coinswap", name = "Coinswap")]
impl Plugin for CoinswapPlugin {
    fn start(&self, host: Arc<dyn HostServices>) -> Result<(), PluginError> {
        // Before anything else that could fail, so coinswap's account of a failed start is
        // visible rather than discarded.
        logging::install(Arc::clone(&host));

        let settings = Settings::load(host.as_ref());
        *lock(&self.host) = Some(Arc::clone(&host));
        *lock(&self.settings) = Some(settings.clone());

        if !settings.enabled {
            host.log(
                LogLevel::Info,
                "Coinswap maker is off. Turn it on from the settings page.".to_string(),
            );
            return Ok(());
        }

        // A bad configuration must not fail the load. The settings page lives in this plugin, so
        // a plugin that refuses to start takes away the only way to fix what broke it.
        if let Err(error) = self.reconcile(&settings) {
            host.log(
                LogLevel::Error,
                format!("The maker did not start: {error}. Fix it on the settings page."),
            );
        }

        Ok(())
    }

    fn stop(&self) {
        let logger = self.host().map_or_else(Logger::silent, Self::logger);
        self.maker.stop(STOP_DEADLINE, &logger);
        // After the stop, so anything coinswap logs on its way down is still delivered.
        logging::detach();
    }

    fn settings_schema(&self) -> UiDocument {
        Document::new("Coinswap maker")
            .alert(
                AlertLevel::Info,
                "Saving restarts the maker. A swap in progress at that moment falls back to its \
                 timelocked refund path, so prefer to save while idle.",
            )
            .form(self.settings().form().submit_label("Save and restart"))
            .into()
    }

    fn pages(&self) -> Vec<PageInfo> {
        vec![PageInfo::new("dashboard", "Maker dashboard")]
    }

    fn page(&self, id: String) -> Result<UiDocument, PluginError> {
        match id.as_str() {
            "dashboard" => Ok(self.dashboard().into()),
            _ => Ok(UiDocument::empty()),
        }
    }

    fn handle(&self, event: HostEvent) -> Result<Vec<PluginAction>, PluginError> {
        match event {
            HostEvent::SettingsUpdated { values } => {
                // Starts from what is stored rather than from defaults, so a secret the operator
                // did not retype keeps its value. The host omits an untouched secret from the
                // submission precisely so that this can happen.
                let mut settings = self.settings();
                settings.update(&values)?;
                settings.check().map_err(PluginError::invalid_input)?;

                *lock(&self.settings) = Some(settings.clone());

                let mut actions = vec![PluginAction::SaveSettings {
                    values: settings.to_values(),
                }];
                actions.push(match self.reconcile(&settings) {
                    Ok(message) => PluginAction::ShowMessage {
                        level: MessageLevel::Success,
                        text: message,
                    },
                    Err(error) => PluginAction::ShowMessage {
                        level: MessageLevel::Error,
                        text: format!("Settings saved, but the maker did not start: {error}"),
                    },
                });
                Ok(actions)
            }

            HostEvent::CommandInvoked { command, .. } => Ok(self.run_command(&command)),

            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard as JSON, which is how the host sees it.
    fn dashboard_json(plugin: &CoinswapPlugin) -> String {
        plugin.page("dashboard".to_string()).unwrap().document_json
    }

    #[test]
    fn the_plugin_offers_a_settings_page_and_a_dashboard() {
        let plugin = CoinswapPlugin::default();
        let pages = plugin.pages();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "dashboard");
        assert!(plugin.settings_schema().document_json.contains("\"form\""));
    }

    #[test]
    fn the_dashboard_renders_before_the_plugin_has_started() {
        // BTCPay serves pages whether or not `start` succeeded. A dashboard that needed a live
        // maker would show an error instead of the settings link the operator needs.
        let plugin = CoinswapPlugin::default();
        assert!(dashboard_json(&plugin).contains("Stopped"));
    }

    #[test]
    fn an_unknown_page_is_empty_rather_than_an_error() {
        let plugin = CoinswapPlugin::default();
        let page = plugin.page("nope".to_string()).unwrap();
        assert!(page.title.is_empty());
    }

    #[test]
    fn a_stopped_maker_offers_no_drain_or_stop_button() {
        // Both act on a running maker, and neither can work without one.
        let json = dashboard_json(&CoinswapPlugin::default());
        assert!(!json.contains("\"drain\""));
        assert!(!json.contains("\"stop\""));
    }

    #[test]
    fn no_page_ever_offers_a_fidelity_bond_command() {
        // A bond must commit to the maker's onion hostname, which comes from a private coinswap
        // method. Any bond this plugin created would be committed to the wrong address and its
        // funds unspendable until the timelock expired, so the command must not exist at all.
        let json = dashboard_json(&CoinswapPlugin::default());
        assert!(!json.contains("\"bond\""));
    }

    #[test]
    fn a_funding_address_is_not_offered_before_the_wallet_is_open() {
        let json = dashboard_json(&CoinswapPlugin::default());
        assert!(!json.contains("\"address\""));
    }

    #[test]
    fn a_disabled_maker_offers_no_start_button_either() {
        // Starting would immediately fail the `enabled` check, so the page says to go to
        // settings instead of offering a button that reports an error.
        let json = dashboard_json(&CoinswapPlugin::default());
        assert!(!json.contains("\"start\""));
        assert!(json.contains("settings page"));
    }

    #[test]
    fn a_command_pressed_before_start_reports_instead_of_panicking() {
        let plugin = CoinswapPlugin::default();
        for command in ["start", "stop", "address", "drain"] {
            let actions = plugin.run_command(command);
            assert!(
                actions.iter().any(|action| matches!(
                    action,
                    PluginAction::ShowMessage {
                        level: MessageLevel::Error,
                        ..
                    }
                )),
                "{command} should report a failure"
            );
        }
    }

    #[test]
    fn an_unknown_command_is_reported_rather_than_ignored() {
        let plugin = CoinswapPlugin::default();
        let actions = plugin.run_command("definitely-not-a-command");
        assert!(actions.iter().any(|action| matches!(
            action,
            PluginAction::ShowMessage {
                level: MessageLevel::Error,
                ..
            }
        )));
    }

    #[test]
    fn a_failed_command_is_logged_as_well_as_shown() {
        // A page message dies with the page. A command that half-moved money needs to leave a
        // trace an operator can find afterwards.
        let plugin = CoinswapPlugin::default();
        let actions = plugin.run_command("address");
        assert!(actions
            .iter()
            .any(|action| matches!(action, PluginAction::Log { .. })));
    }

    #[test]
    fn the_settings_form_never_carries_the_stored_rpc_password() {
        // The contract drops secret values before they reach the browser. Worth asserting here
        // because this plugin is the first one that has a secret to leak.
        let plugin = CoinswapPlugin::default();
        *lock(&plugin.settings) = Some(Settings {
            core_password: "hunter2".to_string(),
            ..Settings::default()
        });

        assert!(!plugin.settings_schema().document_json.contains("hunter2"));
    }

    #[test]
    fn sats_are_grouped_so_a_bond_amount_can_be_read() {
        assert_eq!(CoinswapPlugin::sats(0), "0 sats");
        assert_eq!(CoinswapPlugin::sats(999), "999 sats");
        assert_eq!(CoinswapPlugin::sats(1_000), "1,000 sats");
        assert_eq!(CoinswapPlugin::sats(5_000_000), "5,000,000 sats");
    }

    #[test]
    fn a_rejected_save_leaves_the_running_settings_untouched() {
        // `update` parses into a clone precisely so that a rejection cannot half-apply. Without
        // that, a submission failing on its last field would leave the earlier fields live but
        // unsaved, and the maker would restart onto a configuration nobody chose.
        let plugin = CoinswapPlugin::default();
        *lock(&plugin.settings) = Some(Settings {
            wallet_name: "original".to_string(),
            ..Settings::default()
        });

        let submission = std::collections::HashMap::from([
            ("wallet_name".to_string(), "changed".to_string()),
            // Refused by `check`, and only after the wallet name has already been parsed.
            ("fidelity_timelock".to_string(), "5".to_string()),
        ]);
        let result = plugin.handle(HostEvent::SettingsUpdated { values: submission });

        assert!(result.is_err());
        assert_eq!(plugin.settings().wallet_name, "original");
    }

    #[test]
    fn a_save_that_omits_the_rpc_password_keeps_the_stored_one() {
        // The end-to-end version of the framework fix: the host omits an untouched secret from
        // the submission, and the plugin must read that as "leave it alone".
        let plugin = CoinswapPlugin::default();
        *lock(&plugin.settings) = Some(Settings {
            core_password: "stored-password".to_string(),
            // Needed only so `check` passes and the save is not rejected before it caches.
            tor_auth_password: "stored-tor-password".to_string(),
            ..Settings::default()
        });

        // No host is attached, so the restart fails and is reported, but the parse still happened.
        let _ = plugin.handle(HostEvent::SettingsUpdated {
            values: std::collections::HashMap::from([(
                "core_user".to_string(),
                "someone".to_string(),
            )]),
        });

        assert_eq!(plugin.settings().core_password, "stored-password");
        assert_eq!(plugin.settings().tor_auth_password, "stored-tor-password");
        assert_eq!(plugin.settings().core_user, "someone");
    }
}
