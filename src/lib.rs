//! Coinswap inside BTCPay Server: a maker, a taker, or both.
//!
//! Both roles are off until switched on, and either can run without the other.
//!
//! Each role keeps its own wallet under the plugin's data directory, with its own recovery
//! phrase. Neither is a BTCPay store wallet, and money in them is not money in a store. A swap
//! gains privacy for coins in the taker wallet, so the flow is fund, swap, withdraw elsewhere.
//!
//! Swapping itself is not built yet; the taker's wallet is.
//!
//! - [`settings`] is what the operator fills in, and the one place it becomes a
//!   `MakerServerConfig`.
//! - [`maker`] owns the maker's thread.
//! - [`taker`] owns the taker's wallet, a separate wallet from the maker's.
//! - [`shared`] is what both roles need: the phase, the logger, the lock helpers.
//! - [`logging`] routes openswap's own log output into BTCPay's.
//! - This module is the glue: pages, commands, and reacting to a settings change.

#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod alerts;
pub mod logging;
pub mod maker;
pub mod settings;
pub mod shared;
pub mod taker;
#[cfg(test)]
mod testing;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btcpay_plugin::prelude::*;

use alerts::{Watcher, WatcherSlot};
use maker::{MakerRuntime, Status};
use settings::Settings;
use shared::{lock, unix_now, Logger, Phase};
use taker::{QuoteState, SwapState, TakerRuntime};

/// How long a drain command waits for an idle swap to wind up.
///
/// Short, because the command runs inside the operator's request.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a stop waits for the maker before giving up on it.
///
/// A maker mid-swap has contracts to watch; past this, BTCPay's own shutdown matters more.
const STOP_DEADLINE: Duration = Duration::from_secs(30);

/// Where a notification sends the operator.
///
/// The route `cargo btcpay` generates from the plugin identifier, plus the dashboard page id.
const DASHBOARD_LINK: &str = "/plugins/btcpayserver-plugins-openswap/dashboard";

/// The plugin.
#[derive(Default)]
pub struct CoinswapPlugin {
    maker: Arc<MakerRuntime>,
    taker: TakerRuntime,
    /// Cached so rendering a page does not re-read storage field by field across the FFI
    /// boundary.
    settings: Mutex<Option<Settings>>,
    /// Kept from `start` so a command can log and find the data directory.
    host: Mutex<Option<Arc<dyn HostServices>>>,
    watcher: WatcherSlot,
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
    /// A subdirectory, so the wallet does not sit alongside whatever else the plugin writes.
    fn maker_dir(host: &dyn HostServices) -> PathBuf {
        PathBuf::from(host.data_dir()).join("maker")
    }

    /// Where the taker keeps its wallet. A sibling of the maker's, never the same directory.
    fn taker_dir(host: &dyn HostServices) -> PathBuf {
        PathBuf::from(host.data_dir()).join("taker")
    }

    /// Brings the taker's wallet into line with the settings.
    ///
    /// Separate from `reconcile` so either role's failure cannot stop the other.
    fn reconcile_taker(&self, settings: &Settings) -> Result<String, String> {
        let Some(host) = self.host() else {
            return Err("The plugin has not finished starting yet.".to_string());
        };
        let logger = Self::logger(Arc::clone(&host));

        if !settings.taker_enabled {
            if self.taker.is_live() {
                return Ok(if self.taker.stop(STOP_DEADLINE, &logger) {
                    "Taker wallet closed.".to_string()
                } else {
                    "Taker wallet was asked to close but did not finish in time.".to_string()
                });
            }
            return Ok("Taker is off.".to_string());
        }

        settings.check()?;

        if self.taker.is_live() {
            self.taker.stop(STOP_DEADLINE, &logger);
        }

        let dir = Self::taker_dir(host.as_ref());
        std::fs::create_dir_all(&dir).map_err(|error| {
            format!(
                "Could not create the taker directory at {}: {error}",
                dir.display()
            )
        })?;

        self.taker.start(settings.to_taker_config(dir), logger)?;

        Ok("Taker wallet is opening. Reload the taker page to see it.".to_string())
    }

    /// Builds the taker wallet page.
    fn taker_page(&self) -> Document {
        let settings = self.settings();
        let status = self.taker.status();

        let mut page = Document::new("Coinswap taker");

        // Names which wallet it belongs to: two roles means two phrases.
        if let Some(words) = self.taker.take_new_mnemonic() {
            page = page
                .alert(
                    AlertLevel::Warning,
                    "A new taker wallet was created. Write down the recovery phrase below now. \
                     It is shown once and stored nowhere this page can read again. This is the \
                     taker's wallet, a different wallet from the maker's, with its own phrase.",
                )
                .text(words);
        }

        page = match &status.phase {
            Phase::Failed(reason) => page.alert(
                AlertLevel::Danger,
                format!("The taker wallet did not open: {reason}"),
            ),
            Phase::Starting => page.alert(
                AlertLevel::Info,
                "The taker wallet is opening. It scans the chain on first use, which takes a \
                 while.",
            ),
            Phase::Running => page.alert(
                AlertLevel::Info,
                "This wallet is the taker's own. Coins get here because somebody sent them, and \
                 leave because somebody withdrew them. It is not a BTCPay store wallet, and a \
                 swap gains privacy only for coins held here.",
            ),
            Phase::Stopped if !settings.taker_enabled => page.alert(
                AlertLevel::Info,
                "The taker is off. Turn on \"Enable the taker wallet\" on the settings page.",
            ),
            Phase::Stopped => page.alert(
                AlertLevel::Warning,
                "The taker is enabled but its wallet is not open.",
            ),
        };

        // An interrupted swap leaves funds in timelocked contracts, so this outranks the balance.
        if status.recovery_complete == Some(false) {
            page = page.alert(
                AlertLevel::Warning,
                "A previous swap is still being recovered, so its funds are in contracts until \
                 their timelocks expire. openswap does this itself and needs nothing from you; \
                 a new swap is refused until it finishes, because starting one now would commit \
                 funds on top of funds already committed.",
            );
        }

        let mut stats = Stats::new().card("Status", Self::phase_label(&status.phase));

        if status.wallet_busy {
            stats = stats
                .card("Balances", "Busy")
                .detail("The wallet is locked right now; reload in a moment.");
        }

        if let Some(balances) = &status.balances {
            stats = stats
                .card("Spendable", Self::sats(balances.spendable.to_sat()))
                .card("In swaps", Self::sats(balances.swap.to_sat()))
                .card("In contracts", Self::sats(balances.contract.to_sat()))
                .detail("Locked until a swap finishes or its timelock expires.");
        }

        page = page.stats(stats);

        match self.taker.swap_state() {
            SwapState::Idle => {}
            SwapState::Running { swap_id, since } => {
                page = page.alert(
                    AlertLevel::Warning,
                    format!(
                        "Swap {} is running, {} minutes in. Its funds are committed on chain. \
                         Leave BTCPay running: stopping it now leaves contracts to recover from, \
                         which takes until their timelocks expire.",
                        swap_id.chars().take(16).collect::<String>(),
                        since.elapsed().as_secs() / 60
                    ),
                );
            }
            SwapState::Failed {
                swap_id,
                why,
                committed,
            } => {
                let funds = match committed {
                    Some(false) => {
                        "Nothing left the wallet, so there is nothing to recover and a new quote \
                         can be requested."
                    }
                    Some(true) => {
                        "Funds left the wallet and are held in contracts until their timelocks \
                         expire. This page shows while their recovery is pending."
                    }
                    None => {
                        "Whether funds left the wallet could not be checked. The balance on this \
                         page shows it once the wallet has synced."
                    }
                };
                page = page
                    .alert(
                        AlertLevel::Danger,
                        format!(
                            "Swap {} did not finish: {why} {funds}",
                            swap_id.chars().take(16).collect::<String>()
                        ),
                    )
                    .actions(
                        Actions::new().button(Button::new("clear-swap", "Dismiss and start over")),
                    );
            }
            SwapState::Done(outcome) => {
                page = page
                    .alert(
                        AlertLevel::Success,
                        format!(
                            "Swap {} finished as {}, in {:.0} seconds.",
                            outcome.swap_id.chars().take(16).collect::<String>(),
                            outcome.status,
                            outcome.duration_secs
                        ),
                    )
                    .stats(
                        Stats::new()
                            .card("Sent", Self::sats(outcome.sent_sat))
                            .card("Received", Self::sats(outcome.received_sat))
                            .card("Maker fees", Self::sats(outcome.maker_fees_sat))
                            .card("Mining fee", Self::sats(outcome.mining_fee_sat)),
                    )
                    .actions(
                        Actions::new().button(Button::new("clear-swap", "Clear and start over")),
                    );
            }
        }

        // The quote, above the wallet: it is what the operator is waiting on.
        match self.taker.quote_state() {
            QuoteState::Idle => {}
            QuoteState::Preparing => {
                page = page.alert(
                    AlertLevel::Info,
                    "Preparing a quote. This asks makers over Tor and waits on discovery, so it \
                     takes a while. Reload to see it.",
                );
            }
            QuoteState::Failed(why) => {
                page = page.alert(AlertLevel::Danger, format!("No quote: {why}"));
            }
            QuoteState::Ready(quote) => {
                page = page
                    .alert(
                        AlertLevel::Success,
                        format!(
                            "Quote ready. Sending {} costs {} in fees and delivers {}. Nothing \
                             has been committed yet.",
                            Self::sats(quote.send_sat),
                            Self::sats(quote.total_fee_sat),
                            Self::sats(quote.receive_sat)
                        ),
                    )
                    .stats(
                        Stats::new()
                            .card("Swap", quote.swap_id.chars().take(16).collect::<String>())
                            .card("Sending", Self::sats(quote.send_sat))
                            .card("Fees", Self::sats(quote.total_fee_sat))
                            .card("You receive", Self::sats(quote.receive_sat))
                            .detail("After every hop's fee."),
                    );

                let mut hops =
                    Table::new(["Hop", "Maker", "Locktime (blocks)", "Fee"]).title("Route");
                for (index, hop) in quote.hops.iter().enumerate() {
                    hops = hops.row([
                        (index + 1).to_string(),
                        hop.address.clone(),
                        hop.locktime.to_string(),
                        Self::sats(hop.fee_sat),
                    ]);
                }
                page = page.table(hops).actions(
                    Actions::new()
                        .title("This quote")
                        .button(
                            Button::new("accept-quote", "Accept and swap")
                                .primary()
                                .confirm(format!(
                                    "This commits {} on chain across {} hop(s) and pays {} in \
                                     fees. Once it starts, stopping BTCPay leaves contracts to \
                                     recover from, which takes until their timelocks expire. \
                                     Continue?",
                                    Self::sats(quote.send_sat),
                                    quote.hops.len(),
                                    Self::sats(quote.total_fee_sat)
                                )),
                        )
                        .button(Button::new("discard-quote", "Discard and start over")),
                );
            }
        }

        // Nothing needing the taker is offered while a swap holds it; both would fail on the
        // lock.
        if self.taker.wallet_open() && !self.taker.swap_running() {
            page = page
                .actions(
                    Actions::new()
                        .title("Funding")
                        .button(Button::new("taker-address", "Show a funding address").primary()),
                )
                // A form, not a command: a button carries neither address nor amount. Arrives
                // as `FormSubmitted` because its id is not "settings".
                // A quote is a question, not a spend, so nothing is confirmed here.
                .form(
                    Form::new("quote")
                        .title("Request a swap quote")
                        .number("sats", "Amount to swap (sats)")
                        .required()
                        .range(1, i64::MAX)
                        .number("makers", "Makers in the route")
                        .required()
                        .range(1, 10)
                        .help("More hops cost more and gain more privacy.")
                        .submit_label("Get a quote"),
                )
                .form(
                    Form::new("withdraw")
                        .title("Withdraw")
                        .text("address", "Destination address")
                        .required()
                        .help("Checked against this wallet's network before anything is signed.")
                        .number("sats", "Amount (sats)")
                        .required()
                        .range(1, i64::MAX)
                        .submit_label("Withdraw"),
                );
        }

        page
    }

    /// Brings the maker into line with the settings: running when enabled, stopped when not.
    ///
    /// Returns a message for the operator, who is waiting on a save or a button press.
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

        // Restart rather than reconfigure: `init` consumes the config and the server owns it.
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

        // First on the page: an operator who misses it cannot recover the wallet.
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

        // Swaps first when any still has money on chain: funds sit in a contract until a
        // timelock releases them, and nothing else on this page would say so.
        let now = unix_now();
        let (stale, recovering): (Vec<_>, Vec<_>) = status
            .swaps
            .iter()
            .filter(|swap| swap.funds_at_stake())
            .partition(|swap| swap.is_stale(now));
        if !recovering.is_empty() {
            let total: u64 = recovering.iter().map(|swap| swap.amount_sat).sum();
            page = page.alert(
                AlertLevel::Warning,
                format!(
                    "{} swap(s) did not finish, with {} still in contracts on chain. The maker \
                     recovers these itself once the timelock allows it, which can take a while. \
                     Leave it running: stopping it now delays recovery.",
                    recovering.len(),
                    Self::sats(total)
                ),
            );
        }
        // Separate because the advice is the opposite: waiting helps a swap still inside its
        // timelock, and does nothing for one recovery has stopped advancing.
        if !stale.is_empty() {
            let total: u64 = stale.iter().map(|swap| swap.amount_sat).sum();
            page = page.alert(
                AlertLevel::Warning,
                format!(
                    "{} swap(s) have not changed for over a day, recorded at {} in contracts on \
                     chain. Recovery is not advancing them, so leaving the maker running will \
                     not bring these coins back. BTCPay's log says why on each maker start; look \
                     for \"Reboot recovery failed\". If the coins are not the maker's to claim, \
                     dismiss the swaps below. That only clears them from this page and from \
                     notifications; nothing on chain changes.",
                    stale.len(),
                    Self::sats(total)
                ),
            );
        }

        if !status.swaps.is_empty() {
            let mut swaps = Table::new(["Swap", "Amount", "Phase", "Recovery", "On chain"])
                .title("Unfinished swaps");
            for swap in &status.swaps {
                swaps = swaps.row([
                    swap.id.clone(),
                    Self::sats(swap.amount_sat),
                    swap.phase.clone(),
                    swap.recovery.clone(),
                    if swap.funded { "yes" } else { "no" }.to_string(),
                ]);
            }
            page = page.table(swaps);
        }

        if !stale.is_empty() {
            let mut dismiss = Actions::new().title("Stalled swaps");
            for swap in &stale {
                dismiss = dismiss.button(
                    Button::new(
                        format!("dismiss-swap:{}", swap.tracker_key),
                        format!("Dismiss {}", swap.id),
                    )
                    .destructive(format!(
                        "Dismiss swap {}? It stops appearing here and in notifications. Its coins \
                         stay where they are on chain.",
                        swap.id
                    )),
                );
            }
            page = page.actions(dismiss);
        }

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
    /// A button that cannot work is worse than no button.
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

        // Offered whenever the wallet is open, including after a failed start: the maker cannot
        // start without a funded wallet, so withholding an address would deadlock.
        if self.maker.wallet_open() {
            actions = actions.button(Button::new("address", "Show a funding address"));
        }

        // No fidelity bond command: the bond commits to the maker's onion hostname, which comes
        // from a private openswap method. A button here could only lock funds into an address no
        // taker would use. Renewal happens on the next start.

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

    /// Groups digits: a bond amount in sats is easy to misread by a factor of ten.
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

    /// Sends funds out of the taker wallet.
    ///
    /// The amount is parsed here rather than trusted to the host's own validation.
    fn withdraw(&self, values: &std::collections::HashMap<String, String>) -> Vec<PluginAction> {
        let address = values
            .get("address")
            .map(String::as_str)
            .unwrap_or_default();

        let outcome = match values.get("sats").map(|sats| sats.trim().parse::<u64>()) {
            Some(Ok(sats)) => self
                .taker
                .withdraw(address, sats, None)
                .map(|txid| format!("Withdrawal broadcast. Transaction {txid}.")),
            Some(Err(_)) => Err("The amount must be a whole number of sats.".to_string()),
            None => Err("The amount is missing.".to_string()),
        };

        Self::report(outcome, "withdraw")
    }

    /// Asks for a quote, parsing what the form sent.
    fn request_quote(
        &self,
        values: &std::collections::HashMap<String, String>,
    ) -> Vec<PluginAction> {
        let number = |key: &str| {
            values
                .get(key)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
        };

        let settings = self.settings();
        let outcome = match (number("sats"), number("makers")) {
            (Some(sats), Some(makers)) => {
                let logger = match self.host() {
                    Some(host) => Self::logger(host),
                    None => {
                        return Self::report(
                            Err("The plugin has not finished starting yet.".to_string()),
                            "quote",
                        )
                    }
                };
                self.taker
                    .request_quote(
                        sats,
                        makers as usize,
                        settings.taker_protocol.to_protocol_version(),
                        logger,
                    )
                    .map(|()| {
                        "Preparing a quote. It asks makers over Tor, so reload in a moment."
                            .to_string()
                    })
            }
            _ => Err("The amount and the maker count must both be whole numbers.".to_string()),
        };

        Self::report(outcome, "quote")
    }

    /// Turns an outcome into what the host should do about it.
    ///
    /// Failures are logged as well as shown: a page message dies with the page.
    fn report(outcome: Result<String, String>, what: &str) -> Vec<PluginAction> {
        match outcome {
            Ok(text) => vec![PluginAction::ShowMessage {
                level: MessageLevel::Success,
                text,
            }],
            Err(text) => vec![
                PluginAction::Log {
                    level: LogLevel::Error,
                    message: format!("{what} failed: {text}"),
                },
                PluginAction::ShowMessage {
                    level: MessageLevel::Error,
                    text,
                },
            ],
        }
    }

    /// Runs a button press.
    ///
    /// Every arm reports something: a button that appears to do nothing looks like a failure.
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
            // Does not clear `enabled`: this stops the maker now, and a restart brings it back.
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
            "accept-quote" => match self.host() {
                Some(host) => self.taker.accept_quote(Self::logger(host)).map(|swap_id| {
                    format!(
                        "Swap {} started. It runs for a while; reload to follow it.",
                        swap_id.chars().take(16).collect::<String>()
                    )
                }),
                None => Err("The plugin has not finished starting yet.".to_string()),
            },
            "clear-swap" => self
                .taker
                .clear_swap()
                .map(|()| "Cleared. Request a new quote when you want one.".to_string()),
            "discard-quote" => {
                self.taker.clear_quote();
                Ok("Quote discarded.".to_string())
            }
            "taker-address" => self.taker.receive_address().map(|address| {
                format!(
                    "Send funds to {address} to top up the taker wallet. This is the taker's own \
                     wallet, not a store wallet. The address advances each time."
                )
            }),
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
            other => match other.strip_prefix("dismiss-swap:") {
                Some(id) => self.maker.dismiss_swap(id),
                None => Err(format!("Unknown command: {other}")),
            },
        };

        match outcome {
            Ok(text) => vec![PluginAction::ShowMessage {
                level: MessageLevel::Success,
                text,
            }],
            // Logged as well as shown: a half-completed move must outlive the page.
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
        // Before anything that could fail, so openswap's account of it is not discarded.
        logging::install(Arc::clone(&host));

        let settings = Settings::load(host.as_ref());
        *lock(&self.host) = Some(Arc::clone(&host));
        *lock(&self.settings) = Some(settings.clone());

        // Neither failure fails the load: this plugin owns the only page that can fix it.
        // Reported separately so an operator can see which role is unhappy.
        if settings.enabled {
            if let Err(error) = self.reconcile(&settings) {
                host.log(
                    LogLevel::Error,
                    format!("The maker did not start: {error}. Fix it on the settings page."),
                );
            }
        } else {
            host.log(
                LogLevel::Info,
                "Coinswap maker is off. Turn it on from the settings page.".to_string(),
            );
        }

        self.watcher.replace(Watcher::start(
            Arc::clone(&self.maker),
            Arc::clone(&host),
            DASHBOARD_LINK.to_string(),
            Self::logger(Arc::clone(&host)),
        ));

        if settings.taker_enabled {
            if let Err(error) = self.reconcile_taker(&settings) {
                host.log(
                    LogLevel::Error,
                    format!("The taker wallet did not open: {error}."),
                );
            }
        }

        Ok(())
    }

    fn stop(&self) {
        let logger = self.host().map_or_else(Logger::silent, Self::logger);
        self.watcher.clear();
        // The maker first: it has counterparties waiting, so it gets the larger share of the
        // shutdown budget.
        self.maker.stop(STOP_DEADLINE, &logger);
        self.taker.stop(STOP_DEADLINE, &logger);
        // After both, so anything logged on the way down is still delivered.
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
        vec![
            PageInfo::new("dashboard", "Maker dashboard"),
            PageInfo::new("taker", "Taker wallet"),
        ]
    }

    fn page(&self, id: String) -> Result<UiDocument, PluginError> {
        match id.as_str() {
            "dashboard" => Ok(self.dashboard().into()),
            "taker" => Ok(self.taker_page().into()),
            _ => Ok(UiDocument::empty()),
        }
    }

    fn handle(&self, event: HostEvent) -> Result<Vec<PluginAction>, PluginError> {
        match event {
            HostEvent::SettingsUpdated { values } => {
                // From stored values, not defaults: the host omits an untouched secret from the
                // submission, so it must keep its old value.
                let mut settings = self.settings();
                settings.update(&values)?;
                settings.check().map_err(PluginError::invalid_input)?;

                *lock(&self.settings) = Some(settings.clone());

                let mut actions = vec![PluginAction::SaveSettings {
                    values: settings.to_values(),
                }];

                // Both outcomes reported, or a taker-only save would say nothing about it.
                for (role, outcome) in [
                    ("maker", self.reconcile(&settings)),
                    ("taker", self.reconcile_taker(&settings)),
                ] {
                    actions.push(match outcome {
                        Ok(message) => PluginAction::ShowMessage {
                            level: MessageLevel::Success,
                            text: message,
                        },
                        Err(error) => PluginAction::ShowMessage {
                            level: MessageLevel::Error,
                            text: format!("Settings saved, but the {role} did not start: {error}"),
                        },
                    });
                }
                Ok(actions)
            }

            // Delivered separately from a settings save, so asking for an address cannot be
            // mistaken for reconfiguring the plugin.
            HostEvent::FormSubmitted { form_id, values } => match form_id.as_str() {
                "withdraw" => Ok(self.withdraw(&values)),
                "quote" => Ok(self.request_quote(&values)),
                other => Err(PluginError::invalid_input(format!("unknown form: {other}"))),
            },

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
    fn the_plugin_offers_a_settings_page_and_one_page_per_role() {
        let plugin = CoinswapPlugin::default();
        let ids: Vec<_> = plugin.pages().into_iter().map(|page| page.id).collect();
        assert_eq!(ids, vec!["dashboard", "taker"]);
        assert!(plugin.settings_schema().document_json.contains("\"form\""));
    }

    #[test]
    fn the_dashboard_renders_before_the_plugin_has_started() {
        // BTCPay serves pages whether or not `start` succeeded, and the operator still needs
        // the settings link.
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
        // Any bond this plugin created would commit to the wrong address and lock the funds
        // until its timelock expired.
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
        // Starting would fail the `enabled` check, so the page points at settings instead.
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
        // A page message dies with the page; a half-completed move must leave a trace.
        let plugin = CoinswapPlugin::default();
        let actions = plugin.run_command("address");
        assert!(actions
            .iter()
            .any(|action| matches!(action, PluginAction::Log { .. })));
    }

    #[test]
    fn the_settings_form_never_carries_the_stored_rpc_password() {
        // The contract drops secret values before they reach the browser.
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
        // `update` parses into a clone so a rejection cannot half-apply, leaving the maker on a
        // configuration nobody chose.
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
        // The host omits an untouched secret, which must read as "leave it alone".
        let plugin = CoinswapPlugin::default();
        *lock(&plugin.settings) = Some(Settings {
            core_password: "stored-password".to_string(),
            // Only so `check` passes and the save is not rejected before it caches.
            tor_auth_password: "stored-tor-password".to_string(),
            ..Settings::default()
        });

        // No host attached, so the restart fails, but the parse still happened.
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

    /// The taker page as JSON, which is how the host sees it.
    fn taker_json(plugin: &CoinswapPlugin) -> String {
        plugin.page("taker".to_string()).unwrap().document_json
    }

    #[test]
    fn the_taker_page_renders_before_the_plugin_has_started() {
        let json = taker_json(&CoinswapPlugin::default());
        assert!(json.contains("Stopped"));
        assert!(json.contains("settings page"));
    }

    #[test]
    fn the_taker_page_offers_nothing_that_needs_an_open_wallet() {
        // Both need the wallet, so neither is offered until it is open.
        let json = taker_json(&CoinswapPlugin::default());
        assert!(!json.contains("taker-address"));
        assert!(!json.contains("\"withdraw\""));
    }

    #[test]
    fn the_taker_page_says_the_wallet_is_not_a_store_wallet() {
        // The single most likely misunderstanding: that swapping here privatises store funds.
        let plugin = CoinswapPlugin::default();
        *lock(&plugin.settings) = Some(Settings {
            taker_enabled: true,
            ..Settings::default()
        });
        assert!(taker_json(&plugin).contains("not open"));
    }

    #[test]
    fn a_withdrawal_before_the_wallet_is_open_is_reported_not_attempted() {
        let plugin = CoinswapPlugin::default();
        let actions = plugin.withdraw(&std::collections::HashMap::from([
            ("address".to_string(), "tb1qexample".to_string()),
            ("sats".to_string(), "5000".to_string()),
        ]));

        assert!(actions.iter().any(|action| matches!(
            action,
            PluginAction::ShowMessage {
                level: MessageLevel::Error,
                ..
            }
        )));
        // Logged too, because a withdrawal is money moving.
        assert!(actions
            .iter()
            .any(|action| matches!(action, PluginAction::Log { .. })));
    }

    #[test]
    fn a_withdrawal_amount_that_is_not_a_number_is_refused_by_the_plugin() {
        // The plugin should not depend on the host checking its input.
        let plugin = CoinswapPlugin::default();
        let actions = plugin.withdraw(&std::collections::HashMap::from([
            ("address".to_string(), "tb1qexample".to_string()),
            ("sats".to_string(), "half of it".to_string()),
        ]));
        assert!(actions.iter().any(|action| matches!(
            action,
            PluginAction::ShowMessage { level: MessageLevel::Error, text }
                if text.contains("whole number")
        )));
    }

    #[test]
    fn an_unknown_form_is_refused_rather_than_acted_on() {
        let plugin = CoinswapPlugin::default();
        let result = plugin.handle(HostEvent::FormSubmitted {
            form_id: "not-a-form".to_string(),
            values: std::collections::HashMap::new(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn a_settings_save_reports_on_both_roles() {
        // A save that only changed the taker used to say nothing about the taker at all.
        let plugin = CoinswapPlugin::default();
        *lock(&plugin.settings) = Some(Settings {
            tor_auth_password: "x".to_string(),
            ..Settings::default()
        });

        let actions = plugin
            .handle(HostEvent::SettingsUpdated {
                values: std::collections::HashMap::new(),
            })
            .unwrap();

        let messages = actions
            .iter()
            .filter(|action| matches!(action, PluginAction::ShowMessage { .. }))
            .count();
        assert_eq!(messages, 2, "one message per role");
    }

    #[test]
    fn a_quote_is_not_offered_before_the_wallet_is_open() {
        let json = taker_json(&CoinswapPlugin::default());
        assert!(!json.contains("\"quote\""));
    }

    #[test]
    fn a_quote_request_before_the_wallet_is_open_is_reported() {
        let plugin = CoinswapPlugin::default();
        let actions = plugin.request_quote(&std::collections::HashMap::from([
            ("sats".to_string(), "100000".to_string()),
            ("makers".to_string(), "2".to_string()),
        ]));
        assert!(actions.iter().any(|action| matches!(
            action,
            PluginAction::ShowMessage {
                level: MessageLevel::Error,
                ..
            }
        )));
    }

    #[test]
    fn a_quote_request_with_junk_numbers_is_refused_by_the_plugin() {
        // The plugin should not depend on the host checking its input.
        let plugin = CoinswapPlugin::default();
        for values in [
            [("sats", "lots"), ("makers", "2")],
            [("sats", "100000"), ("makers", "")],
        ] {
            let map: std::collections::HashMap<String, String> = values
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let actions = plugin.request_quote(&map);
            assert!(actions.iter().any(|action| matches!(
                action,
                PluginAction::ShowMessage { level: MessageLevel::Error, text }
                    if text.contains("whole numbers")
            )));
        }
    }

    #[test]
    fn dismissing_a_swap_before_the_maker_wallet_is_open_is_reported() {
        let plugin = CoinswapPlugin::default();
        let actions = plugin.run_command("dismiss-swap:abc123");
        assert!(actions.iter().any(|action| matches!(
            action,
            PluginAction::ShowMessage {
                level: MessageLevel::Error,
                ..
            }
        )));
    }

    #[test]
    fn an_empty_dismissal_is_not_a_known_command() {
        let plugin = CoinswapPlugin::default();
        let actions = plugin.run_command("dismiss-swap");
        assert!(actions.iter().any(|action| matches!(
            action,
            PluginAction::ShowMessage { text, .. } if text.contains("Unknown command")
        )));
    }
}
