<div align="center">

<img src="assets/coinswap.btcpay.svg" alt="Coinswap" width="140">

<h2>Coinswap for BTCPay Server.</h2>

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](#license)

</div>

---

A BTCPay Server plugin that runs an [openswap](https://github.com/citadel-foss/openswap) maker, a
taker, or both, next to your stores.

- **Maker:** earns fees by offering swap liquidity. It holds a fidelity bond, advertises itself
  over Nostr and serves takers over Tor.
- **Taker:** gains privacy for its own coins. Fund the taker wallet, swap through makers, then
  withdraw elsewhere.

Each role has its own wallet and recovery phrase under the plugin's data directory. Neither is a
BTCPay store wallet, so money in them is not money in a store. The plugin is written in Rust with
[btcpay-rs](https://github.com/uqlidi/btcpay-rs).

## Status

**Experimental. Do not use it with real funds.** It has only been tested on signet, and:

- Neither wallet is encrypted on disk.
- It is pinned to an openswap master revision rather than a released version.
- No swap has completed end to end yet.

## Requirements

| | |
|---|---|
| BTCPay Server | 2.4.x on `linux-x64` |
| Chain source | Bitcoin Core with RPC and ZMQ, or an Electrum server |
| Tor | A SOCKS port and a password-protected control port, reachable at `127.0.0.1` from BTCPay |

Building it also needs Rust, the .NET SDK 10.0 and a C/C++ toolchain.

## Usage

Build the plugin package:

```sh
cargo install cargo-btcpay --version 0.1.0-alpha.1
cargo btcpay package
```

That writes `artifacts/BTCPayServer.Plugins.Coinswap/0.1.0.0/BTCPayServer.Plugins.Coinswap.btcpay`.
Upload it in BTCPay under Server Settings, Plugins, and restart BTCPay when it asks.

Then, on the plugin's settings page:

1. Pick the network and the chain source. The network defaults to Regtest, so an unconfigured
   plugin cannot touch real funds.
2. Enter the Tor ports and the control password.
3. Turn on **Run the maker**, **Enable the taker wallet**, or both, and save.

A new wallet shows its recovery phrase once. Write it down before leaving the page.

- **Maker dashboard:** status, balances, the fidelity bond and unfinished swaps. The maker creates
  its bond from a funded wallet, and the page gives a funding address.
- **Taker wallet:** balance, a funding address and withdrawals. Ask for a quote to see the route
  and fees, then accept it to run the swap.

## License

MIT.
