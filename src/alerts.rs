//! Tells the operator about the maker without them having to go and look.
//!
//! The dashboard already reports everything here, but only to someone who opens it. A watcher
//! thread polls instead and raises a BTCPay notification when something changes for the worse.
//!
//! Each condition notifies once when it starts and re-arms when it clears, so a maker that stays
//! down produces one notification rather than one per tick.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use btcpay_plugin::prelude::{HostServices, Notification};

use crate::maker::MakerRuntime;
use crate::shared::{lock, sleep_unless_stopped, Logger, Phase, SendOnDrop};

/// How often the maker is looked at.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How close a fidelity bond may come to maturing before the operator is told.
///
/// About two weeks of blocks, against a timelock the settings hold between 12,960 and 25,920,
/// so the warning arrives with time to create the replacement bond.
const BOND_EXPIRY_WARNING_BLOCKS: u64 = 2_016;

/// How many consecutive ticks an unfinished funded swap must be seen before it counts as stuck.
///
/// Thirty minutes at the poll interval. Swaps take minutes, so anything past this has stopped
/// making progress.
const STUCK_TICKS: u32 = 30;

/// How long a stop waits for the watcher thread.
///
/// Short: it holds nothing a counterparty is waiting on, unlike the maker.
const STOP_DEADLINE: Duration = Duration::from_secs(5);

/// One tick's view of the maker.
pub struct Observation {
    /// What the maker is doing.
    pub phase: Phase,
    /// Spendable balance. `None` when the wallet could not be read.
    pub spendable_sat: Option<u64>,
    /// The smallest swap this maker advertises. `None` when it is not running.
    pub min_swap_sat: Option<u64>,
    /// Blocks until the soonest-expiring bond matures. `None` when there is no bond, or the
    /// chain tip could not be read.
    pub bond_expiry_blocks: Option<u64>,
    /// Swaps with funding on chain that have not finished.
    pub unfinished_funded_swaps: Vec<String>,
}

impl Observation {
    /// A maker that is not running, so nothing but its phase can be read.
    pub fn only_phase(phase: Phase) -> Self {
        Self {
            phase,
            spendable_sat: None,
            min_swap_sat: None,
            bond_expiry_blocks: None,
            unfinished_funded_swaps: Vec::new(),
        }
    }
}

/// What has already been reported, so a condition notifies once rather than every tick.
#[derive(Default)]
pub struct Alerts {
    maker_down: bool,
    liquidity_exhausted: bool,
    bond_expiring: bool,
    ticks_seen: HashMap<String, u32>,
    reported_swaps: HashSet<String>,
}

impl Alerts {
    /// Notifications this observation calls for, given everything seen before it.
    ///
    /// A field that could not be read leaves its condition untouched: an unreadable wallet is
    /// not evidence that the balance is fine, nor that it is not.
    pub fn assess(&mut self, observation: &Observation) -> Vec<Notification> {
        let mut raised = Vec::new();

        if let Phase::Failed(reason) = &observation.phase {
            if !self.maker_down {
                self.maker_down = true;
                raised.push(notification(
                    "The coinswap maker is down",
                    format!(
                        "It stopped with: {reason}. Takers cannot reach it until it is started \
                         again."
                    ),
                ));
            }
        } else {
            self.maker_down = false;
        }

        if let (Some(spendable), Some(minimum)) =
            (observation.spendable_sat, observation.min_swap_sat)
        {
            if spendable < minimum {
                if !self.liquidity_exhausted {
                    self.liquidity_exhausted = true;
                    raised.push(notification(
                        "The coinswap maker has run out of liquidity",
                        format!(
                            "{spendable} sats spendable, below the {minimum} sats it advertises \
                             as its minimum swap, so it cannot accept one."
                        ),
                    ));
                }
            } else {
                self.liquidity_exhausted = false;
            }
        }

        if let Some(blocks) = observation.bond_expiry_blocks {
            if blocks <= BOND_EXPIRY_WARNING_BLOCKS {
                if !self.bond_expiring {
                    self.bond_expiring = true;
                    raised.push(notification(
                        "The coinswap fidelity bond is nearing expiry",
                        format!(
                            "{blocks} blocks until it matures. A maker without a bond is not \
                             chosen by takers, so create the next one before then."
                        ),
                    ));
                }
            } else {
                self.bond_expiring = false;
            }
        }

        raised.extend(self.assess_swaps(&observation.unfinished_funded_swaps));
        raised
    }

    /// A swap reports once and never again, so two records that can never finish cost two
    /// notifications rather than one every half hour.
    fn assess_swaps(&mut self, unfinished: &[String]) -> Vec<Notification> {
        let present: HashSet<&str> = unfinished.iter().map(String::as_str).collect();
        self.ticks_seen
            .retain(|id, _| present.contains(id.as_str()));

        let mut raised = Vec::new();
        for id in unfinished {
            let ticks = self.ticks_seen.entry(id.clone()).or_default();
            *ticks += 1;
            if *ticks >= STUCK_TICKS && self.reported_swaps.insert(id.clone()) {
                raised.push(notification(
                    "A coinswap is stuck",
                    format!(
                        "Swap {id} has funding on chain and has not moved for {} minutes. Its \
                         funds stay in the contract until recovery completes.",
                        STUCK_TICKS as u64 * POLL_INTERVAL.as_secs() / 60
                    ),
                ));
            }
        }
        raised
    }
}

fn notification(title: &str, body: String) -> Notification {
    Notification {
        title: title.to_string(),
        body,
        link: None,
    }
}

/// Polls the maker and raises notifications for what an operator must not miss.
pub struct Watcher {
    stop_requested: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    finished: Receiver<()>,
}

impl Watcher {
    /// Spawns the watcher. `link` is where a notification sends the operator.
    pub fn start(
        maker: Arc<MakerRuntime>,
        host: Arc<dyn HostServices>,
        link: String,
        log: Logger,
    ) -> Self {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let (finished_tx, finished) = mpsc::channel();

        let thread = std::thread::Builder::new()
            .name("openswap-alerts".to_string())
            .spawn({
                let stop_requested = Arc::clone(&stop_requested);
                move || {
                    let _signal = SendOnDrop(finished_tx);
                    let mut alerts = Alerts::default();

                    // Sleeps first: a maker that has just been asked to start is not yet news.
                    while !sleep_unless_stopped(POLL_INTERVAL, &stop_requested) {
                        for mut raised in alerts.assess(&maker.observe()) {
                            raised.link = Some(link.clone());
                            if let Err(error) = host.emit_notification(raised) {
                                log.error(&format!("Could not raise a notification: {error:?}"));
                            }
                        }
                    }
                }
            })
            .ok();

        Self {
            stop_requested,
            thread,
            finished,
        }
    }

    /// Asks the watcher to stop and waits a short while for it.
    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else {
            return;
        };
        if self.finished.recv_timeout(STOP_DEADLINE) == Err(RecvTimeoutError::Timeout) {
            return;
        }
        let _ = thread.join();
    }
}

/// Holds the watcher for the plugin, so starting one twice cannot leave the first running.
#[derive(Default)]
pub struct WatcherSlot(Mutex<Option<Watcher>>);

impl WatcherSlot {
    /// Stops whatever is in the slot and puts `watcher` there.
    pub fn replace(&self, watcher: Watcher) {
        let mut slot = lock(&self.0);
        if let Some(mut previous) = slot.take() {
            previous.stop();
        }
        *slot = Some(watcher);
    }

    /// Stops and drops whatever is in the slot.
    pub fn clear(&self) {
        if let Some(mut watcher) = lock(&self.0).take() {
            watcher.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(spendable: u64, minimum: u64) -> Observation {
        Observation {
            phase: Phase::Running,
            spendable_sat: Some(spendable),
            min_swap_sat: Some(minimum),
            bond_expiry_blocks: None,
            unfinished_funded_swaps: Vec::new(),
        }
    }

    #[test]
    fn a_failed_maker_notifies_once() {
        let mut alerts = Alerts::default();
        let failed = Observation::only_phase(Phase::Failed("no backend".to_string()));

        let first = alerts.assess(&failed);
        assert_eq!(first.len(), 1);
        assert!(first[0].body.contains("no backend"));

        assert!(alerts.assess(&failed).is_empty());
    }

    #[test]
    fn a_maker_that_recovers_and_fails_again_notifies_again() {
        let mut alerts = Alerts::default();
        let failed = Observation::only_phase(Phase::Failed("no backend".to_string()));

        assert_eq!(alerts.assess(&failed).len(), 1);
        assert!(alerts
            .assess(&Observation::only_phase(Phase::Running))
            .is_empty());
        assert_eq!(alerts.assess(&failed).len(), 1);
    }

    #[test]
    fn a_stopped_maker_is_not_news() {
        let mut alerts = Alerts::default();
        assert!(alerts
            .assess(&Observation::only_phase(Phase::Stopped))
            .is_empty());
    }

    #[test]
    fn liquidity_below_the_advertised_minimum_notifies_once() {
        let mut alerts = Alerts::default();

        let raised = alerts.assess(&running(9_000, 10_000));
        assert_eq!(raised.len(), 1);
        assert!(raised[0].body.contains("9000"));

        assert!(alerts.assess(&running(9_000, 10_000)).is_empty());
    }

    #[test]
    fn refunding_the_wallet_re_arms_the_liquidity_alert() {
        let mut alerts = Alerts::default();

        assert_eq!(alerts.assess(&running(9_000, 10_000)).len(), 1);
        assert!(alerts.assess(&running(50_000, 10_000)).is_empty());
        assert_eq!(alerts.assess(&running(9_000, 10_000)).len(), 1);
    }

    #[test]
    fn liquidity_exactly_at_the_minimum_is_enough() {
        let mut alerts = Alerts::default();
        assert!(alerts.assess(&running(10_000, 10_000)).is_empty());
    }

    #[test]
    fn an_unreadable_wallet_decides_nothing() {
        let mut alerts = Alerts::default();
        assert_eq!(alerts.assess(&running(9_000, 10_000)).len(), 1);

        // Wallet busy: neither a fresh alert nor a clearing of the old one.
        let unreadable = Observation::only_phase(Phase::Running);
        assert!(alerts.assess(&unreadable).is_empty());

        assert!(alerts.assess(&running(9_000, 10_000)).is_empty());
    }

    #[test]
    fn a_bond_inside_the_warning_window_notifies_once() {
        let mut alerts = Alerts::default();
        let expiring = Observation {
            bond_expiry_blocks: Some(BOND_EXPIRY_WARNING_BLOCKS),
            ..running(50_000, 10_000)
        };

        assert_eq!(alerts.assess(&expiring).len(), 1);
        assert!(alerts.assess(&expiring).is_empty());
    }

    #[test]
    fn a_bond_with_time_left_is_not_news() {
        let mut alerts = Alerts::default();
        let healthy = Observation {
            bond_expiry_blocks: Some(BOND_EXPIRY_WARNING_BLOCKS + 1),
            ..running(50_000, 10_000)
        };

        assert!(alerts.assess(&healthy).is_empty());
    }

    #[test]
    fn a_replacement_bond_re_arms_the_expiry_alert() {
        let mut alerts = Alerts::default();
        let expiring = Observation {
            bond_expiry_blocks: Some(100),
            ..running(50_000, 10_000)
        };
        let replaced = Observation {
            bond_expiry_blocks: Some(20_000),
            ..running(50_000, 10_000)
        };

        assert_eq!(alerts.assess(&expiring).len(), 1);
        assert!(alerts.assess(&replaced).is_empty());
        assert_eq!(alerts.assess(&expiring).len(), 1);
    }

    #[test]
    fn a_swap_is_stuck_only_after_it_stops_moving() {
        let mut alerts = Alerts::default();
        let swapping = Observation {
            unfinished_funded_swaps: vec!["abc123".to_string()],
            ..running(50_000, 10_000)
        };

        for _ in 1..STUCK_TICKS {
            assert!(alerts.assess(&swapping).is_empty());
        }
        let raised = alerts.assess(&swapping);
        assert_eq!(raised.len(), 1);
        assert!(raised[0].body.contains("abc123"));
    }

    #[test]
    fn a_stuck_swap_notifies_once_however_long_it_stays() {
        let mut alerts = Alerts::default();
        let swapping = Observation {
            unfinished_funded_swaps: vec!["abc123".to_string()],
            ..running(50_000, 10_000)
        };

        for _ in 0..STUCK_TICKS {
            alerts.assess(&swapping);
        }
        for _ in 0..STUCK_TICKS {
            assert!(alerts.assess(&swapping).is_empty());
        }
    }

    #[test]
    fn a_swap_that_finishes_and_returns_starts_counting_again() {
        let mut alerts = Alerts::default();
        let swapping = Observation {
            unfinished_funded_swaps: vec!["abc123".to_string()],
            ..running(50_000, 10_000)
        };
        let idle = running(50_000, 10_000);

        for _ in 1..STUCK_TICKS {
            alerts.assess(&swapping);
        }
        assert!(alerts.assess(&idle).is_empty());
        assert!(alerts.assess(&swapping).is_empty());
    }

    #[test]
    fn each_stuck_swap_is_reported_separately() {
        let mut alerts = Alerts::default();
        let swapping = Observation {
            unfinished_funded_swaps: vec!["abc123".to_string(), "def456".to_string()],
            ..running(50_000, 10_000)
        };

        for _ in 1..STUCK_TICKS {
            alerts.assess(&swapping);
        }
        assert_eq!(alerts.assess(&swapping).len(), 2);
    }

    #[test]
    fn conditions_are_reported_together() {
        let mut alerts = Alerts::default();
        let bad = Observation {
            phase: Phase::Failed("no backend".to_string()),
            spendable_sat: Some(0),
            min_swap_sat: Some(10_000),
            bond_expiry_blocks: Some(10),
            unfinished_funded_swaps: Vec::new(),
        };

        assert_eq!(alerts.assess(&bad).len(), 3);
    }

    #[test]
    fn a_watcher_stops_without_being_waited_out() {
        let maker = Arc::new(MakerRuntime::new());
        let host = Arc::new(crate::testing::Discard);
        let mut watcher = Watcher::start(
            maker,
            host,
            "/plugins/test/dashboard".to_string(),
            Logger::silent(),
        );

        let started = std::time::Instant::now();
        watcher.stop();
        assert!(started.elapsed() < STOP_DEADLINE);
    }
}
