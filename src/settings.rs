//! Operator-facing configuration, and its translation into a `MakerServerConfig`.
//!
//! [`Settings`] is flat and form-renderable; `MakerServerConfig` is nested and typed.
//! [`Settings::to_maker_config`] is the only place the two meet.

use std::path::PathBuf;

use btcpay_plugin::prelude::*;
// Via openswap's re-exports, so a version bump cannot leave two incompatible `Network` types.
use openswap::{
    bitcoin::Network,
    bitcoind::bitcoincore_rpc::Auth,
    maker::MakerServerConfig,
    protocol::common_messages::ProtocolVersion,
    taker::{api::ConnectionType, TakerInitConfig},
    wallet::{BackendConfig, CoreRpcConfig, ElectrumConfig},
};

// Mirrored from openswap `src/wallet/fidelity.rs`, which keeps them behind a private module.
// Can drift from upstream. openswap halves these under `integration-test`, which we do not enable.
const MIN_FIDELITY_TIMELOCK: u32 = 12_960; // about 3 months
const MAX_FIDELITY_TIMELOCK: u32 = 25_920; // about 6 months

/// From openswap `src/maker/api.rs`.
const MIN_SWAP_AMOUNT: u64 = 10_000;

/// Reduces a Core RPC endpoint to the bare `host:port` openswap wants.
///
/// openswap builds `http://{url}/wallet/{name}` itself, so a scheme here yields
/// `http://http://host:port/...` and a DNS error naming no cause.
fn host_port(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    // Anything after the host and port is a path, which openswap appends itself.
    value.split('/').next().unwrap_or(value).trim().to_string()
}

/// How the taker reaches makers.
#[derive(BtcpayChoice, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Reach {
    /// Through the Tor SOCKS proxy. The only way to reach an onion address, which is what a
    /// maker advertises, so this is the default.
    #[default]
    #[choice(value = "tor", label = "Tor")]
    Tor,
    /// Direct TCP. Cannot reach an onion address at all, so it is only useful against a maker
    /// on a clearnet address, which in practice means a local test.
    #[choice(value = "clearnet", label = "Clearnet (test only)")]
    Clearnet,
}

impl Reach {
    fn to_connection_type(self) -> ConnectionType {
        match self {
            Reach::Tor => ConnectionType::Tor,
            Reach::Clearnet => ConnectionType::Clearnet,
        }
    }
}

/// Which swap protocol the taker asks for.
#[derive(BtcpayChoice, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Protocol {
    /// ECDSA contracts with script-evaluated HTLCs. Every maker supports it.
    #[default]
    #[choice(value = "legacy", label = "Legacy")]
    Legacy,
    /// Taproot MuSig2 with scriptless contracts. Only makers advertising it are eligible,
    /// which is most of them: a maker that supports both advertises "Unified".
    #[choice(value = "taproot", label = "Taproot")]
    Taproot,
}

impl Protocol {
    pub(crate) fn to_protocol_version(self) -> ProtocolVersion {
        match self {
            Protocol::Legacy => ProtocolVersion::Legacy,
            Protocol::Taproot => ProtocolVersion::Taproot,
        }
    }
}

/// Where the maker gets its chain data from.
#[derive(BtcpayChoice, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Backend {
    /// The natural choice inside BTCPay, which already runs one.
    #[default]
    #[choice(value = "core", label = "Bitcoin Core (RPC)")]
    CoreRpc,
    /// For an operator who does not want the maker touching their node.
    #[choice(value = "electrum", label = "Electrum server")]
    Electrum,
}

/// Which chain to run on.
///
/// Deliberately not every network `bitcoin::Network` has: an unexercised one loses money.
#[derive(BtcpayChoice, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Chain {
    /// Real money.
    #[choice(value = "mainnet", label = "Mainnet")]
    Mainnet,
    /// The public test network.
    #[choice(value = "testnet4", label = "Testnet4")]
    Testnet4,
    /// The test network whose blocks are signed rather than mined.
    #[choice(value = "signet", label = "Signet")]
    Signet,
    /// The default, so an unconfigured plugin cannot touch real funds.
    #[default]
    #[choice(value = "regtest", label = "Regtest")]
    Regtest,
}

impl Chain {
    fn to_network(self) -> Network {
        match self {
            Chain::Mainnet => Network::Bitcoin,
            Chain::Testnet4 => Network::Testnet4,
            Chain::Signet => Network::Signet,
            Chain::Regtest => Network::Regtest,
        }
    }
}

/// Everything the operator configures, for both roles.
///
/// Chain source and Tor settings are shared; everything else is per role, including the wallet
/// name -- openswap keeps maker and taker wallets as separate files with separate seeds.
///
/// Both roles default to off: neither should start because somebody clicked install.
// Every field carries a `label` and `help`, which is the operator-facing description and
// the one that has to be right. A doc comment would be a second copy to keep in step.
#[allow(missing_docs)]
#[derive(BtcpaySettings, Clone, Debug)]
pub struct Settings {
    #[setting(
        label = "Run the maker",
        help = "Off until the settings below are right. Turning this off stops the maker; it does not release a fidelity bond."
    )]
    pub enabled: bool,

    #[setting(label = "Network")]
    pub chain: Chain,

    /// Selects which of the two blocks below is used.
    #[setting(label = "Chain source")]
    pub backend: Backend,

    // --- Bitcoin Core, used when backend is CoreRpc ---
    /// Normalised by [`host_port`] before it reaches openswap.
    #[setting(
        label = "Core RPC host and port",
        help = "Host and port, for example 127.0.0.1:18443 . A http:// prefix is accepted and \
                removed. Ignored when the chain source is Electrum."
    )]
    pub core_url: String,

    #[setting(label = "Core RPC username")]
    pub core_user: String,

    #[setting(label = "Core RPC password", secret)]
    pub core_password: String,

    #[setting(
        label = "Core ZMQ address",
        help = "Block and transaction notifications, for example tcp://127.0.0.1:28332 . The watchtower needs this to see a counterparty broadcasting a contract."
    )]
    pub core_zmq_addr: String,

    // --- Electrum, used when backend is Electrum ---
    // Unlike the Core endpoint, this one MUST carry a scheme: it picks the transport and is
    // passed to `ElectrumClient::from_config` verbatim.
    #[setting(
        label = "Electrum URL",
        help = "For example ssl://electrum.example.org:50002 . Ignored when the chain source is Bitcoin Core."
    )]
    pub electrum_url: String,

    #[setting(
        label = "Electrum SOCKS5 proxy",
        help = "Leave empty to connect directly. An onion address needs a proxy, usually 127.0.0.1:9050 ."
    )]
    pub electrum_socks5: String,

    // --- Wallet ---
    #[setting(
        label = "Wallet name",
        help = "Names the maker's own wallet file under the plugin data directory. This is not a BTCPay store wallet and BTCPay never sees it.",
        required
    )]
    pub wallet_name: String,

    // --- Network ---
    #[setting(
        label = "Maker port",
        help = "The port takers reach this maker on. Changing it after a fidelity bond exists strands the bond, because a bond commits to the address it was created for."
    )]
    pub network_port: u32,

    #[setting(label = "Tor SOCKS port")]
    pub socks_port: u32,

    #[setting(label = "Tor control port")]
    pub control_port: u32,

    #[setting(label = "Tor control password", secret)]
    pub tor_auth_password: String,

    // --- Fee policy ---
    #[setting(
        label = "Base fee (sats)",
        help = "Charged per swap regardless of size."
    )]
    pub base_fee: u32,

    #[setting(
        label = "Amount fee (basis points)",
        help = "Proportional fee on the swap amount. 100 basis points is 1 percent."
    )]
    pub amount_relative_fee_bps: u32,

    #[setting(
        label = "Time fee (basis points)",
        help = "Proportional fee for the time funds stay locked."
    )]
    pub time_relative_fee_bps: u32,

    #[setting(
        label = "Minimum swap (sats)",
        help = "Swaps smaller than this are refused."
    )]
    pub min_swap_amount: u32,

    #[setting(
        label = "Required confirmations",
        help = "Confirmations the maker waits for before treating a funding transaction as final."
    )]
    pub required_confirms: u32,

    // --- Fidelity bond ---
    #[setting(
        label = "Fidelity bond amount (sats)",
        help = "Locked in a timelocked output to prove this maker has skin in the game. These funds are unspendable until the timelock expires, and no command here can shorten that."
    )]
    pub fidelity_amount: u32,

    #[setting(
        label = "Fidelity bond timelock (blocks)",
        help = "How long the bond stays locked, in blocks. openswap accepts roughly three to six \
                months; the exact range is reported if this is out of bounds."
    )]
    pub fidelity_timelock: u32,

    // --- Taker ---
    /// Whether the taker's wallet should be open.
    ///
    /// Independent of the maker: an operator may want one, the other, or both.
    #[setting(
        label = "Enable the taker wallet",
        help = "Opens a second wallet, separate from the maker's and from every BTCPay store \
                wallet. It has its own recovery phrase."
    )]
    pub taker_enabled: bool,

    /// Names the taker's wallet file under the plugin data directory.
    #[setting(
        label = "Taker wallet name",
        help = "Must differ from the maker's wallet name: they are two wallets with two seeds.",
        required
    )]
    pub taker_wallet_name: String,

    /// How the taker reaches makers.
    #[setting(
        label = "Reach makers over",
        help = "Makers advertise onion addresses, which only Tor can reach. Clearnet is for a \
                local test against a maker that is not behind Tor."
    )]
    pub taker_reach: Reach,

    #[setting(
        label = "Swap protocol",
        help = "Which protocol the taker asks makers for. Legacy is the older path every maker \
                supports. Taproot needs makers that advertise it."
    )]
    pub taker_protocol: Protocol,
}

impl Default for Settings {
    fn default() -> Self {
        // openswap's own defaults, except `enabled`.
        let reference = MakerServerConfig::default();
        Self {
            enabled: false,
            chain: Chain::Regtest,
            backend: Backend::CoreRpc,
            core_url: "127.0.0.1:18443".to_string(),
            core_user: String::new(),
            core_password: String::new(),
            core_zmq_addr: "tcp://127.0.0.1:28332".to_string(),
            electrum_url: String::new(),
            electrum_socks5: String::new(),
            wallet_name: "btcpay-maker".to_string(),
            network_port: u32::from(reference.network_port),
            socks_port: u32::from(reference.socks_port),
            control_port: u32::from(reference.control_port),
            tor_auth_password: reference.tor_auth_password.clone(),
            base_fee: reference.base_fee as u32,
            amount_relative_fee_bps: (reference.amount_relative_fee_pct * 100.0) as u32,
            time_relative_fee_bps: (reference.time_relative_fee_pct * 100.0) as u32,
            min_swap_amount: reference.min_swap_amount as u32,
            required_confirms: reference.required_confirms,
            fidelity_amount: reference.fidelity_amount as u32,
            fidelity_timelock: reference.fidelity_timelock,
            taker_enabled: false,
            taker_protocol: Protocol::Legacy,
            // Distinct from the maker's default on purpose: one wallet name for two wallets
            // would be an operator's worst afternoon.
            taker_wallet_name: "btcpay-taker".to_string(),
            taker_reach: Reach::Tor,
        }
    }
}

impl Settings {
    /// Rejects a configuration the maker could not run with, naming what is wrong.
    ///
    /// Cross-field rules only; the derive already validates individual fields.
    pub fn check(&self) -> Result<(), String> {
        match self.backend {
            Backend::CoreRpc => {
                let endpoint = host_port(&self.core_url);
                if endpoint.is_empty() {
                    return Err(
                        "Core RPC host and port is required when the chain source is Bitcoin \
                         Core."
                            .to_string(),
                    );
                }
                // openswap uses a plain http client, so a missing port silently becomes 80.
                match endpoint.rsplit_once(':') {
                    Some((host, port))
                        if !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0) => {}
                    _ => {
                        return Err(format!(
                            "Core RPC host and port must look like host:port, for example \
                             127.0.0.1:18443. Got \"{endpoint}\"."
                        ))
                    }
                }
            }
            Backend::Electrum => {
                if self.electrum_url.trim().is_empty() {
                    return Err(
                        "Electrum URL is required when the chain source is an Electrum server."
                            .to_string(),
                    );
                }
            }
        }

        if self.wallet_name.trim().is_empty() {
            return Err("Wallet name is required.".to_string());
        }

        if self.taker_enabled {
            if self.taker_wallet_name.trim().is_empty() {
                return Err("Taker wallet name is required.".to_string());
            }
            // Two openswap wallets on one file, and one Bitcoin Core watch-only wallet driven by
            // both, each treating the other's coins as its own. Refuse rather than discover it.
            if self.taker_wallet_name.trim() == self.wallet_name.trim() {
                return Err(
                    "The taker wallet name must differ from the maker's. They are two separate \
                     wallets, and sharing a name would point both at one file."
                        .to_string(),
                );
            }
        }

        for (label, port) in [
            ("Maker port", self.network_port),
            ("Tor SOCKS port", self.socks_port),
            ("Tor control port", self.control_port),
        ] {
            if port == 0 || port > u32::from(u16::MAX) {
                return Err(format!("{label} must be between 1 and 65535."));
            }
        }

        // openswap authenticates to Tor by password, so an empty one cannot work. Caught here
        // because its own failure is the opaque "Failed to retrieve ephemeral onion service
        // details".
        if self.tor_auth_password.is_empty() {
            return Err(
                "Tor control password is required. Tor refuses to open a control port with no \
                 authentication, and openswap authenticates with a password, so this must match \
                 the HashedControlPassword the Tor instance was configured with."
                    .to_string(),
            );
        }

        if !(MIN_FIDELITY_TIMELOCK..=MAX_FIDELITY_TIMELOCK).contains(&self.fidelity_timelock) {
            return Err(format!(
                "Fidelity bond timelock must be between {MIN_FIDELITY_TIMELOCK} and \
                 {MAX_FIDELITY_TIMELOCK} blocks, which is roughly three to six months. \
                 {} is outside what openswap accepts.",
                self.fidelity_timelock
            ));
        }
        if u64::from(self.min_swap_amount) < MIN_SWAP_AMOUNT {
            return Err(format!(
                "Minimum swap must be at least {MIN_SWAP_AMOUNT} sats."
            ));
        }
        if u64::from(self.fidelity_amount) < MIN_SWAP_AMOUNT {
            return Err(format!(
                "Fidelity bond amount must be at least {MIN_SWAP_AMOUNT} sats to be worth \
                 anything to a taker."
            ));
        }

        Ok(())
    }

    /// The chain source, in the shape openswap wants.
    ///
    /// Shared by both roles rather than built twice: they talk to the same node, and two copies
    /// of this would be two places for a change to be forgotten.
    ///
    /// `wallet_name` is a parameter and not `self.wallet_name` because the Core backend names a
    /// **watch-only wallet inside Bitcoin Core**, and the two roles must not share one. If they
    /// did, each would see the other's UTXOs as its own.
    fn backend_config(&self, wallet_name: &str) -> BackendConfig {
        match self.backend {
            Backend::CoreRpc => BackendConfig::CoreRpc(CoreRpcConfig {
                // Bare host:port, never a URL: openswap adds the scheme and the wallet path.
                url: host_port(&self.core_url),
                auth: Auth::UserPass(self.core_user.clone(), self.core_password.clone()),
                // The Core watch-only wallet this role drives, kept distinct from the other
                // role's and from any store wallet BTCPay owns.
                wallet_name: wallet_name.trim().to_string(),
                zmq_addr: self.core_zmq_addr.trim().to_string(),
            }),
            Backend::Electrum => BackendConfig::Electrum(ElectrumConfig {
                url: self.electrum_url.trim().to_string(),
                socks5: Some(self.electrum_socks5.trim().to_string())
                    .filter(|proxy| !proxy.is_empty()),
                timeout: None,
                poll_interval_secs: None,
                max_retries: 3,
            }),
        }
    }

    /// Builds the config the taker's wallet opens with.
    ///
    /// Shares the chain source and the Tor settings with the maker, and nothing else: a separate
    /// wallet name and a separate data directory, so the two wallets cannot touch each other.
    pub fn to_taker_config(&self, data_dir: PathBuf) -> TakerInitConfig {
        // `..default()` for the same reason the maker's config uses it: `nostr_relays` and
        // anything openswap adds later keep openswap's own value rather than whatever zero
        // happens to mean.
        TakerInitConfig {
            data_dir: Some(data_dir),
            backend: self.backend_config(&self.taker_wallet_name),
            wallet_name: self.taker_wallet_name.trim().to_string(),
            // Range-checked in `check`, so these casts cannot truncate a port to zero.
            control_port: Some(self.control_port as u16),
            tor_auth_password: Some(self.tor_auth_password.clone()),
            socks_port: self.socks_port as u16,
            password: None,
            connection_type: self.taker_reach.to_connection_type(),
            ..TakerInitConfig::default()
        }
    }

    /// Builds the config the maker actually runs on.
    pub fn to_maker_config(&self, data_dir: PathBuf) -> MakerServerConfig {
        // The trailing `..default()` keeps openswap's own value for fields not set here, and
        // for anything it adds later.
        MakerServerConfig {
            data_dir,
            network: self.chain.to_network(),
            wallet_name: self.wallet_name.trim().to_string(),

            // Range-checked in `check`, so this cast cannot silently truncate a port to zero.
            network_port: self.network_port as u16,
            socks_port: self.socks_port as u16,
            control_port: self.control_port as u16,
            tor_auth_password: self.tor_auth_password.clone(),

            base_fee: u64::from(self.base_fee),
            amount_relative_fee_pct: f64::from(self.amount_relative_fee_bps) / 100.0,
            time_relative_fee_pct: f64::from(self.time_relative_fee_bps) / 100.0,
            min_swap_amount: u64::from(self.min_swap_amount),
            required_confirms: self.required_confirms,

            fidelity_amount: u64::from(self.fidelity_amount),
            fidelity_timelock: self.fidelity_timelock,

            backend: self.backend_config(&self.wallet_name),

            ..MakerServerConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Settings {
        Settings {
            enabled: true,
            wallet_name: "test-maker".to_string(),
            tor_auth_password: "hunter2".to_string(),
            ..Settings::default()
        }
    }

    #[test]
    fn a_fresh_install_does_not_run_a_maker() {
        // Installing must not bind a port or lock funds.
        assert!(!Settings::default().enabled);
    }

    #[test]
    fn a_fresh_install_defaults_to_regtest() {
        assert_eq!(Settings::default().chain, Chain::Regtest);
    }

    #[test]
    fn core_settings_need_an_endpoint() {
        let settings = Settings {
            backend: Backend::CoreRpc,
            core_url: "  ".to_string(),
            ..valid()
        };
        assert!(settings
            .check()
            .unwrap_err()
            .contains("Core RPC host and port"));
    }

    #[test]
    fn electrum_settings_need_a_url_and_ignore_the_core_one() {
        let settings = Settings {
            backend: Backend::Electrum,
            core_url: "127.0.0.1:18443".to_string(),
            electrum_url: String::new(),
            ..valid()
        };
        assert!(settings.check().unwrap_err().contains("Electrum URL"));
    }

    #[test]
    fn a_port_outside_the_u16_range_is_refused_rather_than_truncated() {
        // Truncating 65536 to 0 would bind a random port and strand any bond.
        let settings = Settings {
            network_port: 65_536,
            ..valid()
        };
        assert!(settings.check().is_err());
    }

    #[test]
    fn basis_points_become_a_percentage() {
        let settings = Settings {
            amount_relative_fee_bps: 250,
            ..valid()
        };
        let config = settings.to_maker_config(PathBuf::from("/tmp/x"));
        assert_eq!(config.amount_relative_fee_pct, 2.5);
    }

    #[test]
    fn the_configured_data_dir_reaches_the_maker() {
        // Otherwise the wallet lands in ~/.openswap, outside the mounted volume, and is lost
        // on the next deploy.
        let dir = PathBuf::from("/var/lib/btcpay/plugins/openswap");
        let config = valid().to_maker_config(dir.clone());
        assert_eq!(config.data_dir, dir);
    }

    #[test]
    fn the_chain_choice_selects_the_bitcoin_network() {
        let config = Settings {
            chain: Chain::Mainnet,
            ..valid()
        }
        .to_maker_config(PathBuf::from("/tmp/x"));
        assert_eq!(config.network, Network::Bitcoin);
    }

    #[test]
    fn an_empty_electrum_proxy_means_no_proxy_rather_than_an_empty_one() {
        let settings = Settings {
            backend: Backend::Electrum,
            electrum_url: "tcp://localhost:50001".to_string(),
            electrum_socks5: "   ".to_string(),
            ..valid()
        };
        match settings.to_maker_config(PathBuf::from("/tmp/x")).backend {
            BackendConfig::Electrum(electrum) => assert_eq!(electrum.socks5, None),
            _ => panic!("expected an Electrum backend"),
        }
    }

    #[test]
    fn a_timelock_outside_what_openswap_accepts_is_refused_at_save_time() {
        // Otherwise it is only caught when "Create fidelity bond" fails.
        for timelock in [0, MIN_FIDELITY_TIMELOCK - 1, MAX_FIDELITY_TIMELOCK + 1] {
            let settings = Settings {
                fidelity_timelock: timelock,
                ..valid()
            };
            let message = settings.check().unwrap_err();
            assert!(message.contains("timelock"), "{timelock}: {message}");
            // The message states the real bounds.
            assert!(
                message.contains("12960") && message.contains("25920"),
                "{message}"
            );
        }
    }

    #[test]
    fn openswaps_own_default_timelock_passes_the_check() {
        // Catches the mirrored bounds drifting from upstream. Goes through `valid()` because
        // `Settings::default()` deliberately does not pass: it has no Tor control password.
        let settings = valid();
        assert_eq!(
            settings.fidelity_timelock,
            Settings::default().fidelity_timelock
        );
        assert!(settings.check().is_ok(), "{:?}", settings.check());
    }

    #[test]
    fn an_empty_tor_password_is_refused_at_save_time() {
        // Otherwise it fails at onion-service publication, naming neither Tor nor the password.
        let settings = Settings {
            tor_auth_password: String::new(),
            ..valid()
        };
        let message = settings.check().unwrap_err();
        assert!(message.contains("Tor control password"), "{message}");
    }

    #[test]
    fn a_swap_below_the_protocol_minimum_is_refused() {
        let settings = Settings {
            min_swap_amount: 500,
            ..valid()
        };
        assert!(settings.check().unwrap_err().contains("10000"));
    }

    #[test]
    fn every_port_is_range_checked_not_just_the_maker_one() {
        // All three are cast to u16 in `to_maker_config`, so any of them could truncate to zero.
        for settings in [
            Settings {
                socks_port: 70_000,
                ..valid()
            },
            Settings {
                control_port: 0,
                ..valid()
            },
        ] {
            assert!(settings.check().is_err());
        }
    }

    #[test]
    fn a_blank_wallet_name_is_refused_because_it_names_a_file() {
        let settings = Settings {
            wallet_name: "   ".to_string(),
            ..valid()
        };
        assert!(settings.check().is_err());
    }

    #[test]
    fn a_pasted_url_is_reduced_to_host_and_port() {
        // Broke a live maker: openswap prepends the scheme, yielding `http://http://...`.
        assert_eq!(
            host_port("http://bitcoind-signet:38332"),
            "bitcoind-signet:38332"
        );
        assert_eq!(host_port("https://node.example:8332"), "node.example:8332");
        assert_eq!(host_port("  127.0.0.1:18443  "), "127.0.0.1:18443");
        assert_eq!(host_port("127.0.0.1:18443/"), "127.0.0.1:18443");
        // openswap appends the wallet path itself, so a supplied one has to go.
        assert_eq!(
            host_port("http://127.0.0.1:18443/wallet/other"),
            "127.0.0.1:18443"
        );
    }

    #[test]
    fn a_pasted_url_reaches_the_maker_as_host_and_port() {
        // The normalisation has to happen on the way into the config, not only in the check.
        let settings = Settings {
            core_url: "http://bitcoind-signet:38332".to_string(),
            ..valid()
        };
        match settings.to_maker_config(PathBuf::from("/tmp/x")).backend {
            BackendConfig::CoreRpc(core) => assert_eq!(core.url, "bitcoind-signet:38332"),
            _ => panic!("expected a Core backend"),
        }
    }

    #[test]
    fn an_endpoint_without_a_port_is_refused_rather_than_defaulting_to_80() {
        // A missing port silently becomes 80.
        for endpoint in [
            "127.0.0.1",
            "http://bitcoind-signet",
            "localhost:",
            ":18443",
        ] {
            let settings = Settings {
                core_url: endpoint.to_string(),
                ..valid()
            };
            let message = settings.check().unwrap_err();
            assert!(
                message.contains("host:port"),
                "{endpoint} should be refused: {message}"
            );
        }
    }

    #[test]
    fn a_port_that_is_not_a_port_is_refused() {
        let settings = Settings {
            core_url: "127.0.0.1:not-a-port".to_string(),
            ..valid()
        };
        assert!(settings.check().is_err());
    }

    #[test]
    fn the_default_endpoint_is_in_the_format_openswap_wants() {
        // The first version of this default carried a scheme.
        let default = Settings::default().core_url;
        assert!(!default.contains("://"), "default must not carry a scheme");
        assert_eq!(host_port(&default), default);
    }

    #[test]
    fn the_two_wallets_may_not_share_a_name() {
        // One name would mean one wallet file and one Core watch-only wallet driven by both
        // roles, each seeing the other's coins as its own.
        let settings = Settings {
            taker_enabled: true,
            wallet_name: "same".to_string(),
            taker_wallet_name: "same".to_string(),
            ..valid()
        };
        assert!(settings.check().unwrap_err().contains("must differ"));
    }

    #[test]
    fn a_name_collision_is_ignored_while_the_taker_is_off() {
        // Nothing opens the taker wallet, so there is nothing to collide with, and refusing here
        // would block a maker-only operator over a field they never filled in.
        let settings = Settings {
            taker_enabled: false,
            wallet_name: "same".to_string(),
            taker_wallet_name: "same".to_string(),
            ..valid()
        };
        assert!(settings.check().is_ok(), "{:?}", settings.check());
    }

    #[test]
    fn the_defaults_give_the_two_roles_different_wallets() {
        let defaults = Settings::default();
        assert_ne!(defaults.wallet_name, defaults.taker_wallet_name);
    }

    #[test]
    fn each_role_drives_its_own_core_watch_only_wallet() {
        // Sharing one would give each role the other's UTXOs.
        let settings = Settings {
            taker_enabled: true,
            ..valid()
        };
        let maker = match settings.to_maker_config(PathBuf::from("/tmp/m")).backend {
            BackendConfig::CoreRpc(core) => core.wallet_name,
            _ => panic!("expected a Core backend"),
        };
        let taker = match settings.to_taker_config(PathBuf::from("/tmp/t")).backend {
            BackendConfig::CoreRpc(core) => core.wallet_name,
            _ => panic!("expected a Core backend"),
        };
        assert_ne!(maker, taker);
    }

    #[test]
    fn the_taker_gets_its_own_data_dir_and_the_shared_tor_settings() {
        let settings = Settings {
            taker_enabled: true,
            ..valid()
        };
        let config = settings.to_taker_config(PathBuf::from("/var/lib/x/taker"));
        assert_eq!(config.data_dir, Some(PathBuf::from("/var/lib/x/taker")));
        assert_eq!(config.wallet_name, settings.taker_wallet_name);
        assert_eq!(config.socks_port, settings.socks_port as u16);
        assert_eq!(
            config.tor_auth_password.as_deref(),
            Some(settings.tor_auth_password.as_str())
        );
    }

    #[test]
    fn the_taker_reaches_makers_over_tor_by_default() {
        // A maker advertises an onion address, which clearnet cannot reach at all.
        assert_eq!(Settings::default().taker_reach, Reach::Tor);
    }

    #[test]
    fn the_swap_protocol_defaults_to_legacy() {
        // Every maker supports Legacy, so an operator who never touches this gets a swap that
        // can route. Taproot is opt-in.
        assert_eq!(Settings::default().taker_protocol, Protocol::Legacy);
    }

    #[test]
    fn both_protocols_map_to_openswaps_own_type() {
        assert_eq!(
            Protocol::Legacy.to_protocol_version(),
            ProtocolVersion::Legacy
        );
        assert_eq!(
            Protocol::Taproot.to_protocol_version(),
            ProtocolVersion::Taproot
        );
    }
}
