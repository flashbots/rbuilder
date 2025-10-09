# rbuilder

[![CI status](https://github.com/flashbots/rbuilder/actions/workflows/checks.yaml/badge.svg?branch=develop)](https://github.com/flashbots/rbuilder/actions/workflows/checks.yaml)
[![Telegram Chat](https://img.shields.io/endpoint?color=neon&logo=telegram&label=Chat&url=https%3A%2F%2Ftg.sumanjay.workers.dev%2Fflashbots_rbuilder)](https://t.me/flashbots_rbuilder)
[![GitHub Release](https://img.shields.io/github/v/release/flashbots/rbuilder?label=Release)](https://github.com/flashbots/rbuilder/releases)

rbuilder is an open-source, blazingly fast, cutting edge implementation of a Ethereum MEV-Boost block builder written in Rust.
It is designed to provide a delightful developer experience, enabling community members to contribute and researchers to use rbuilder to study block building.

#### Features
- **Multiple algorithms**: Can be upgraded to handle several block building algorithms. The included algorithm builds blocks by sorting the orders by either effective gas price or total profit and then trying (they might fail!) to execute them to create the block. For more details see [ordering_builder.rs](crates/rbuilder/src/building/builders/ordering_builder.rs)
- **Backtesting**: support for quick and easy backtesting on mempool transactions and other data
- **Bundle merging**: bundles that target transactions which have already been included in a pending block can be dropped if they are marked in `reverting_tx_hashes`.
- **Smart nonce management**: identifies and smartly handles nonce dependencies between bundles and transactions
- **Using [Reth](https://github.com/paradigmxyz/reth/)**: leverages fast, efficient and user-friendly Ethereum node written in Rust
- Reproducible builds

## Running rbuilder

rbuilder can be run in two modes:
- **Backtesting**: build blocks on historical data. rbuilder leverages the mempool-dumpster's
open database of transactions to let anyone easily backtest block building on previous blocks.
- **Live**: build and submit blocks to MEV-Boost relays in real time.

### Backtesting
rbuilder supports backtesting against historical blocks.
It does this by using the [mempool-dumpster](https://mempool-dumpster.flashbots.net/index.html) for historical mempool transactions to let anyone run rbuilder.
If you have historical bundles, you can also plug that in for testing purposes.
Moreover, rbuilder pulls the on-chain block to compare local block building against what actually landed.
Finally, rbuilder stores historical data locally in an SQLite database to support rapid testing and iteration,

For more details on how to use rbuilder for backtesting, see https://github.com/flashbots/rbuilder/wiki/Noob-Guide-for-Backtesting

### Live

To run rbuilder you need:
* Reth node for state. (`reth_datadir`)
* Reth node must expose ipc interface for mempool tx subscription (`el_node_ipc_path`).
* CL node that triggers new payload events (it must be additionally configured to trigger payload event every single time).
* Source of bundles that sends `eth_sendBundle`, `mev_sendBundle`, `eth_sendRawTransaction` as JSON rpc calls. (`jsonrpc_server_port`)
  (by default rbuilder will take raw txs from the reth node mempool)
* Relays so submit to (`relays`)

A sample configuration for running Lighthouse and triggering payload events would be:
```
./target/maxperf/lighthouse bn \
    --network mainnet \
    --execution-endpoint http://localhost:8551 \
    --execution-jwt /secrets/jwt.hex \
    --checkpoint-sync-url https://mainnet.checkpoint.sigp.io \
    --disable-deposit-contract-sync \
    --http \
    --http-port 3500 \
    --always-prepare-payload \
    --prepare-payload-lookahead 8000 \
    --suggested-fee-recipient 0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045
```

Additionally, you can:
* configure block processor API as a sink for submitted blocks (`blocks_processor_url`)
* setup Prometheus / Grafana for metrics (served on `telemetry_port` + `/debug/metrics/prometheus`)
* record traces of all events happening inside the builder (`tracing_path`, served on `telemetry_port` + `/debug/tracing/{start,stop}`)


Running:
1. Prepare config file based on the [`config-live-example.toml`](./examples/config/rbuilder/config-live-example.toml)
2. Run `rbuilder run PATH_TO_CONFIG_FILE`
See [`config file fields`](./docs/CONFIG.md) for more detailed info.
**Warning**: Even if they are rare, before running a builder you should be aware of [`reorg losses`](./docs/REORG_LOSSES.md).

### Benchmarking

rbuilder has a solid initial benchmarking setup (based on [Criterion.rs](https://github.com/bheisler/criterion.rs)).

- All PRs receive a [benchmark report like this](https://flashbots-rbuilder-ci-stats.s3.us-east-2.amazonaws.com/benchmark/3b22d52-f468712/report/index.html).
- You can run benchmarks with `make bench` and open the Criterion-generated report with `make bench-report-open`.
- Benchmarks are located in [`crates/rbuilder/benches`](./crates/rbuilder/benches/). We'd love to add more meaningful benchmarks there!
- Let us know about further improvement ideas and additional relevant benchmarks.

### Testing with a fake relay

This repo includes a `test-relay` tool for testing live builders without submitting blocks to live production relays. This standalone binary implements the MEV-Boost Relay API required for builder to function. The test-relay only performs block validation and compares profits between builders who submit blocks to it, without actually sending blocks to the network.

The test relay exposes several Prometheus metrics about the blocks it received.

Example of how to use test relay:

```bash
./test-relay \
    --relay "https://boost-relay-hoodi.flashbots.net" \
    --validation-url "http://localhost:8545" \
    --cl-clients "http://localhost:5052" 
```
For relay flag, instead of Hoodi, you can use:

- Mainnet: [https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net/](https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net/)
- Sepolia: [https://boost-relay-sepolia.flashbots.net/](https://boost-relay-sepolia.flashbots.net/)

When validation-url is passed, blocks produced by the `rbuilder` are validated by the EL node. This node must support Flashbots validation API (e.g. `flashbots_validateBuilderSubmissionV3`).

For rbuilder to use fake (`test-relay`), config.toml must include it as (one of) the relays. Example:

```TOML
[[relays]]
name = "flashbots-test"
url = "http://localhost:80"
priority = 0
```

### End-to-end local testing

You can use [builder-playground](https://github.com/flashbots/builder-playground) to deploy a fully functional local setup for the builder ([Lighthouse](https://github.com/sigp/lighthouse) consensus client (proposer + validator) + [Reth](https://github.com/paradigmxyz/reth/) execution client + [MEV-Boost-Relay](https://github.com/flashbots/mev-boost-relay))) to test rbuilder.

First, start [builder-playground](https://github.com/flashbots/builder-playground):

```bash
git clone git@github.com:flashbots/builder-playground.git
cd builder-playground
go run main.go
```

Next, update [`config-playground.toml`](./examples/config/rbuilder/config-playground.toml) with fully-qualified paths for entries containing `$HOME`:

```bash
# replaces '$HOME' with the actual value of "$HOME"
sed -i "s|\$HOME|$HOME|g" ./examples/config/rbuilder/config-playground.toml
```

Then run `rbuilder` using the `config-playground.toml` config file:

```bash
cargo run --bin rbuilder run ./examples/config/rbuilder/config-playground.toml
```

You can query the local relay for proposed blocks like this:

```bash
curl http://localhost:5555/relay/v1/data/bidtraces/proposer_payload_delivered
```

### Reproducible builds

You only need to set the `SOURCE_DATE_EPOCH` environment variable to ensure that the build is reproducible:

```bash
# Use last commit timestamp as the build date
$ export SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)

# build #1
$ rm -rf target/
$ cargo build --release
$ sha256sum target/release/rbuilder
d92ac33b94e16ed4a035b9dd52108fe78bd9bb160a91fced8e439f59b84c3207  target/release/rbuilder

# build #2
$ rm -rf target/
$ cargo build --release
$ sha256sum target/release/rbuilder
d92ac33b94e16ed4a035b9dd52108fe78bd9bb160a91fced8e439f59b84c3207  target/release/rbuilder
```

---

## Release Stability and Development Process

rbuilder is running in production at Flashbots since Q1 2024, and is reasonably stable. It is under active (and sometimes heavy) development, as we are constantly adding new features and improvements.

We encourage users to choose the version that best fits their needs: the latest stable release for production use, or the `develop` branch for those who want to test the latest features and are comfortable with potential instability.

### Develop Branch
The `develop` branch is our main integration branch for ongoing development:
- We frequently merge pull requests into this branch.
- While we strive for quality, the `develop` branch may occasionally be unstable.
- There are no guarantees that code in this branch is fully tested or production-ready.

### Stable Releases
For users seeking stability:
- We recommend using the latest tagged release.
- Each release undergoes thorough testing in production environments before publication.
- Tagged releases offer a higher level of reliability and are suitable for production use.
- You can find the stable releases at [github.com/flashbots/rbuilder/releases](https://github.com/flashbots/rbuilder/releases)

### Release Cadence

We plan to cut a stable release at least once a month, but this may vary depending on the volume of changes and the stability of the codebase. To get notified, watch the repository, and you'll get an email notification on new releases.

---

## Contributing

We welcome contributions to rbuilder! Our contributor guidelines can be found in [`CONTRIBUTING.md`](./CONTRIBUTING.md).


Start by cloning the repo, and running a few common commands:

```bash
git clone git@github.com:flashbots/rbuilder.git
cd rbuilder

# Run linter
make lint

# Run tests
make test

# Run benchmarks and open the report
make bench
make bench-report-open
```

---

## Security

See [`SECURITY.md`](./SECURITY.md)

---

## Acknowledgements

Big shoutout to the [Reth](https://github.com/paradigmxyz/reth) team for building a kick-ass Ethereum node.


---

## Various notes

`make build` builds a bunch of additional binaries:


| Binary                      | Description                                                                                           |
|-----------------------------|-------------------------------------------------------------------------------------------------------|
| `rbuilder`                  | Live block builder                                                                                    |
| `backtest-build-block`      | Run backtests for a single block                                                                      |
| `backtest-build-range`      | Run backtests for a range of block                                                                    |
| `backtest-fetch`            | Download data for backtesting                                                                         |
| `dummy-builder`             | Simple sample builder to show how to plugin a custom `BlockBuildingSink` and `BlockBuildingAlgorithm` |
| `misc-relays-slot`          | Shows info about winning bid for the block                                                            |
| `debug-bench-machine`       | Tests execution performance                                                                           |
| `debug-order-input`         | Observe input of the bundles and transactions                                                         |
| `debug-order-sim`           | Observe simulation of the bundles and transactions                                                    |
| `debug-slot-data-generator` | Shows new payload jobs coming from CL with attached data from relays.                                 |
| `test-relay`                | Test MEV-boost relay that accepts blocks and validates them.                                          |
