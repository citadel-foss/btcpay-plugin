//! Operator-facing configuration, and its translation into a `MakerServerConfig`.
//!
//! Two shapes exist here for a reason. [`Settings`] is what an operator fills in: flat, all
//! strings and integers, every field renderable in a form. [`MakerServerConfig`] is what
//! coinswap wants: nested, typed, with an `Amount` and a `Network` and a backend enum.
//! [`Settings::to_maker_config`] is the only place the two meet, so a form change cannot
//! quietly fail to reach the maker.

use std::path::PathBuf;

use btcpay_plugin::prelude::*;
// Taken from coinswap's own re-exports rather than depending on bitcoin and bitcoind
// directly, so a version bump there cannot leave two incompatible `Network` types in scope.
use coinswap::{
    bitcoin::Network,
    bitcoind::bitcoincore_rpc::Auth,
    maker::MakerServerConfig,
    wallet::{BackendConfig, CoreRpcConfig, ElectrumConfig},
};

// Bounds coinswap enforces, mirrored here because they live behind a private module and cannot
// be imported. Checking them at save time tells the operator immediately, rather than letting a
// bad value sit in storage until bond creation fails on it much later. The cost is three numbers
// that can drift from upstream, so they are named and pointed at their source; making them
// public upstream would remove the copy entirely.
//
// From coinswap `src/wallet/fidelity.rs`. Note that coinswap halves these under its
// `integration-test` feature, which this plugin does not enable.
const MIN_FIDELITY_TIMELOCK: u32 = 12_960; // about 3 months
const MAX_FIDELITY_TIMELOCK: u32 = 25_920; // about 6 months

/// From coinswap `src/maker/api.rs`.
const MIN_SWAP_AMOUNT: u64 = 10_000;

/// Reduces a Core RPC endpoint to the bare `host:port` coinswap wants.
///
/// coinswap builds the URL itself, as `http://{url}/wallet/{name}`. So a value that already
/// carries a scheme becomes `http://http://host:port/wallet/...`, the hostname parses as `http`,
/// and the maker fails with a DNS lookup error that says nothing about the real cause. That is
/// worth a few lines to prevent rather than document: a field asking for an endpoint will be
/// given a URL, because that is what every other Bitcoin tool asks for.
fn host_port(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    // Anything after the host and port is a path, which coinswap appends itself.
    value.split('/').next().unwrap_or(value).trim().to_string()
}

/// Where the maker gets its chain data from.
#[derive(BtcpayChoice, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Backend {
    /// Bitcoin Core over JSON-RPC. The natural choice inside BTCPay, which already runs one.
    #[default]
    #[choice(value = "core", label = "Bitcoin Core (RPC)")]
    CoreRpc,
    /// An Electrum server. For an operator who does not want the maker touching their node.
    #[choice(value = "electrum", label = "Electrum server")]
    Electrum,
}

/// Which chain to run on.
///
/// Deliberately not every network `bitcoin::Network` has: offering a network the maker has
/// never been exercised on is an invitation to lose money on it.
#[derive(BtcpayChoice, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Chain {
    /// Real money.
    #[choice(value = "mainnet", label = "Mainnet")]
    Mainnet,
    /// Public test chain.
    #[choice(value = "testnet4", label = "Testnet4")]
    Testnet4,
    /// Signet.
    #[choice(value = "signet", label = "Signet")]
    Signet,
    /// Local regtest. The default, so an unconfigured plugin cannot touch real funds.
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

/// Everything the operator configures.
///
/// `enabled` defaults to false, so installing the plugin does not start a maker. A maker binds
/// a port, locks funds into a fidelity bond and advertises itself to takers; none of that
/// should happen because somebody clicked install.
#[derive(BtcpaySettings, Clone, Debug)]
pub struct Settings {
    /// Whether the maker should be running. See the note on [`Settings`] for why this defaults to false.
    #[setting(
        label = "Run the maker",
        help = "Off until the settings below are right. Turning this off stops the maker; it does not release a fidelity bond."
    )]
    pub enabled: bool,

    /// Which chain to run on.
    #[setting(label = "Network")]
    pub chain: Chain,

    /// Where to get chain data from. Selects which of the two blocks below is used.
    #[setting(label = "Chain source")]
    pub backend: Backend,

    // --- Bitcoin Core, used when backend is CoreRpc ---
    /// Bitcoin Core's JSON-RPC host and port, without a scheme. Normalised by [`host_port`]
    /// before it reaches coinswap. Used only when `backend` is [`Backend::CoreRpc`].
    #[setting(
        label = "Core RPC host and port",
        help = "Host and port, for example 127.0.0.1:18443 . A http:// prefix is accepted and \
                removed. Ignored when the chain source is Electrum."
    )]
    pub core_url: String,

    /// Bitcoin Core RPC username.
    #[setting(label = "Core RPC username")]
    pub core_user: String,

    /// Bitcoin Core RPC password. Never rendered back into the form.
    #[setting(label = "Core RPC password", secret)]
    pub core_password: String,

    /// Bitcoin Core's ZMQ endpoint, which the watchtower reads block and transaction
    /// notifications from.
    #[setting(
        label = "Core ZMQ address",
        help = "Block and transaction notifications, for example tcp://127.0.0.1:28332 . The watchtower needs this to see a counterparty broadcasting a contract."
    )]
    pub core_zmq_addr: String,

    // --- Electrum, used when backend is Electrum ---
    // Note the asymmetry with the Core field above, which is coinswap's and not a slip here: the
    // Core endpoint must NOT carry a scheme, because coinswap adds one, while this one MUST, and
    // its scheme chooses the transport. It is passed to `ElectrumClient::from_config` verbatim.
    /// Electrum server endpoint, including a `tcp://` or `ssl://` scheme. Used only when
    /// `backend` is [`Backend::Electrum`].
    #[setting(
        label = "Electrum URL",
        help = "For example ssl://electrum.example.org:50002 . Ignored when the chain source is Bitcoin Core."
    )]
    pub electrum_url: String,

    /// SOCKS5 proxy to reach the Electrum server through. Empty means connect directly,
    /// which cannot reach an onion address.
    #[setting(
        label = "Electrum SOCKS5 proxy",
        help = "Leave empty to connect directly. An onion address needs a proxy, usually 127.0.0.1:9050 ."
    )]
    pub electrum_socks5: String,

    // --- Wallet ---
    /// Names the maker's wallet file under the plugin data directory.
    #[setting(
        label = "Wallet name",
        help = "Names the maker's own wallet file under the plugin data directory. This is not a BTCPay store wallet and BTCPay never sees it.",
        required
    )]
    pub wallet_name: String,

    // --- Network ---
    /// The port takers reach this maker on. A fidelity bond commits to it.
    #[setting(
        label = "Maker port",
        help = "The port takers reach this maker on. Changing it after a fidelity bond exists strands the bond, because a bond commits to the address it was created for."
    )]
    pub network_port: u32,

    /// Tor SOCKS proxy port.
    #[setting(label = "Tor SOCKS port")]
    pub socks_port: u32,

    /// Tor control port.
    #[setting(label = "Tor control port")]
    pub control_port: u32,

    /// Tor control port password. Never rendered back into the form.
    #[setting(label = "Tor control password", secret)]
    pub tor_auth_password: String,

    // --- Fee policy ---
    /// Flat fee per swap, in sats.
    #[setting(
        label = "Base fee (sats)",
        help = "Charged per swap regardless of size."
    )]
    pub base_fee: u32,

    /// Fee proportional to the swap amount, in basis points.
    #[setting(
        label = "Amount fee (basis points)",
        help = "Proportional fee on the swap amount. 100 basis points is 1 percent."
    )]
    pub amount_relative_fee_bps: u32,

    /// Fee proportional to how long funds stay locked, in basis points.
    #[setting(
        label = "Time fee (basis points)",
        help = "Proportional fee for the time funds stay locked."
    )]
    pub time_relative_fee_bps: u32,

    /// Swaps below this size are refused, in sats.
    #[setting(
        label = "Minimum swap (sats)",
        help = "Swaps smaller than this are refused."
    )]
    pub min_swap_amount: u32,

    /// Confirmations before a funding transaction counts as final.
    #[setting(
        label = "Required confirmations",
        help = "Confirmations the maker waits for before treating a funding transaction as final."
    )]
    pub required_confirms: u32,

    // --- Fidelity bond ---
    /// Size of the fidelity bond, in sats. Locked until the timelock expires.
    #[setting(
        label = "Fidelity bond amount (sats)",
        help = "Locked in a timelocked output to prove this maker has skin in the game. These funds are unspendable until the timelock expires, and no command here can shorten that."
    )]
    pub fidelity_amount: u32,

    /// How many blocks the fidelity bond stays locked for.
    #[setting(
        label = "Fidelity bond timelock (blocks)",
        help = "How long the bond stays locked, in blocks. coinswap accepts roughly three to six \
                months; the exact range is reported if this is out of bounds."
    )]
    pub fidelity_timelock: u32,
}

impl Default for Settings {
    fn default() -> Self {
        // Mostly mirrors coinswap's own defaults so an operator who changes nothing gets what
        // makerd would have given them. The exception is `enabled`.
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
        }
    }
}

impl Settings {
    /// Rejects a configuration the maker could not run with, naming what is wrong.
    ///
    /// Separate from `from_values()` validation because these are cross-field rules: a Core URL
    /// is only required when the backend is Core, so no single field can check it. The derive
    /// validates fields; this validates the combination.
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
                // A port is required rather than defaulted. coinswap hands the value to a plain
                // http client, so a missing port would quietly become port 80 and fail as a
                // connection error with nothing pointing at the cause.
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

        for (label, port) in [
            ("Maker port", self.network_port),
            ("Tor SOCKS port", self.socks_port),
            ("Tor control port", self.control_port),
        ] {
            if port == 0 || port > u32::from(u16::MAX) {
                return Err(format!("{label} must be between 1 and 65535."));
            }
        }

        // Tor will not open a control port without an authentication method, and coinswap
        // authenticates by password rather than cookie, so an empty one cannot work. It is worth
        // catching here because coinswap's failure for a rejected password is the opaque
        // "Failed to retrieve ephemeral onion service details", which points nowhere near the
        // cause. The default is empty because coinswap's is.
        if self.tor_auth_password.is_empty() {
            return Err(
                "Tor control password is required. Tor refuses to open a control port with no \
                 authentication, and coinswap authenticates with a password, so this must match \
                 the HashedControlPassword the Tor instance was configured with."
                    .to_string(),
            );
        }

        if !(MIN_FIDELITY_TIMELOCK..=MAX_FIDELITY_TIMELOCK).contains(&self.fidelity_timelock) {
            return Err(format!(
                "Fidelity bond timelock must be between {MIN_FIDELITY_TIMELOCK} and \
                 {MAX_FIDELITY_TIMELOCK} blocks, which is roughly three to six months. \
                 {} is outside what coinswap accepts.",
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

    /// Builds the config the maker actually runs on.
    ///
    /// Starts from coinswap's defaults rather than a zeroed struct, so a field coinswap adds
    /// later gets its intended default instead of whatever zero happens to mean.
    pub fn to_maker_config(&self, data_dir: PathBuf) -> MakerServerConfig {
        // The trailing `..default()` is deliberate: `rpc_port`, `supported_protocols`,
        // `nostr_relays` and anything coinswap adds later keep coinswap's own value rather than
        // whatever zero happens to mean for them.
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

            backend: match self.backend {
                Backend::CoreRpc => BackendConfig::CoreRpc(CoreRpcConfig {
                    // Bare host:port, never a URL: coinswap adds the scheme and the wallet path.
                    url: host_port(&self.core_url),
                    auth: Auth::UserPass(self.core_user.clone(), self.core_password.clone()),
                    // The Core watch-only wallet the maker drives, kept distinct from any store
                    // wallet BTCPay owns.
                    wallet_name: self.wallet_name.trim().to_string(),
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
            },

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
        // Installing must not bind a port or lock funds. This is the whole reason `enabled`
        // exists, so it is worth a test that fails loudly if the default is ever flipped.
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
        // Otherwise the maker writes its wallet to ~/.coinswap inside the BTCPay container,
        // where it is outside the mounted data volume and lost on the next deploy.
        let dir = PathBuf::from("/var/lib/btcpay/plugins/coinswap");
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
    fn a_timelock_outside_what_coinswap_accepts_is_refused_at_save_time() {
        // Otherwise the value sits in storage looking accepted, and the operator finds out only
        // when they press "Create fidelity bond" and it fails.
        for timelock in [0, MIN_FIDELITY_TIMELOCK - 1, MAX_FIDELITY_TIMELOCK + 1] {
            let settings = Settings {
                fidelity_timelock: timelock,
                ..valid()
            };
            let message = settings.check().unwrap_err();
            assert!(message.contains("timelock"), "{timelock}: {message}");
            // The message states the real bounds rather than leaving the operator guessing.
            assert!(
                message.contains("12960") && message.contains("25920"),
                "{message}"
            );
        }
    }

    #[test]
    fn coinswaps_own_default_timelock_passes_the_check() {
        // A guard against the mirrored bounds drifting away from upstream: if coinswap's default
        // ever falls outside the range copied here, one of the two is wrong. Checked through
        // `valid()` rather than `Settings::default()`, because the defaults deliberately do not
        // pass: a Tor control password cannot be invented on the operator's behalf.
        let settings = valid();
        assert_eq!(
            settings.fidelity_timelock,
            Settings::default().fidelity_timelock
        );
        assert!(settings.check().is_ok(), "{:?}", settings.check());
    }

    #[test]
    fn an_empty_tor_password_is_refused_at_save_time() {
        // Otherwise the maker starts, gets as far as publishing its onion service, and fails
        // with an error that names neither Tor nor the password.
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
        // The value that actually broke a live maker. coinswap prepends the scheme, so leaving
        // this one in place produced `http://http://bitcoind-signet:38332/wallet/...` and a DNS
        // error naming no cause.
        assert_eq!(
            host_port("http://bitcoind-signet:38332"),
            "bitcoind-signet:38332"
        );
        assert_eq!(host_port("https://node.example:8332"), "node.example:8332");
        assert_eq!(host_port("  127.0.0.1:18443  "), "127.0.0.1:18443");
        assert_eq!(host_port("127.0.0.1:18443/"), "127.0.0.1:18443");
        // coinswap appends the wallet path itself, so a supplied one has to go.
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
        // coinswap hands the value to a plain http client, so a missing port becomes port 80 and
        // surfaces as a connection failure with nothing pointing at the cause.
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
    fn the_default_endpoint_is_in_the_format_coinswap_wants() {
        // Guards the mistake directly: the first version of this default carried a scheme.
        let default = Settings::default().core_url;
        assert!(!default.contains("://"), "default must not carry a scheme");
        assert_eq!(host_port(&default), default);
    }
}
