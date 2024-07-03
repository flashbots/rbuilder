# rbuilder

[![CI status](https://github.com/flashbots/rbuilder/workflows/Checks/badge.svg)](https://github.com/flashbots/rbuilder/actions/workflows/checks.yaml)
[![Telegram Chat](https://img.shields.io/endpoint?color=neon&logo=telegram&label=Chat&url=https%3A%2F%2Ftg.sumanjay.workers.dev%2Fflashbots_rbuilder)](https://t.me/flashbots_rbuilder)
[![GitHub Release](https://img.shields.io/github/v/release/flashbots/rbuilder?label=Release)](https://github.com/flashbots/rbuilder/releases)

rbuilder is an open-source, blazingly fast, cutting edge implementation of a Ethereum MEV-Boost block builder written in Rust.
It is designed to provide a delightful developer experience, enabling community members to contribute and researchers to use rbuilder to study block building.

#### Features
- **Multiple algorithms**: build Ethereum blocks by sorting for either effective gas price `mev_gas_price` or total profit `max_profit`
- **Backtesting**: support for quick and easy backtesting on mempool transactions and other data
- **Bundle merging**: bundles that target transactions which have already been included in a pending block can be dropped if they are marked in `reverting_tx_hashes`.
- **Smart nonce management**: identifies and smartly handles nonce dependencies between bundles and transactions
- **Using [Reth](https://github.com/paradigmxyz/reth/)**: leverages fast, efficient and user-friendly Ethereum node written in Rust

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
* CL node that triggers new payload events (it must be additionally configured to trigger payload event every single time)
* Source of bundles that sends `eth_sendBundle`, `mev_sendBundle`, `eth_sendRawTransaction` as JSON rpc calls. (`jsonrpc_server_port`)
  (by default rbuilder will take raw txs from the reth node mempool)
* Relays so submit to (`relays`)
* Alternatively it can submit to the block validation API if run in the dry run mode (`dry_run`, `dry_run_validation_url`)

Additionally, you can:
* configure block processor API as a sink for submitted blocks (`blocks_processor_url`)
* setup Prometheus / Grafana for metrics (served on `telemetry_port` + `/debug/metrics/prometheus`)
* record traces of all events happening inside the builder (`tracing_path`, served on `telemetry_port` + `/debug/tracing/{start,stop}`)


Running:
1. Prepare config file based on the `config-live-example.toml`
2. Run `rbuilder run PATH_TO_CONFIG_FILE`

### Benchmarking

rbuilder has a solid initial benchmarking setup (based on [Criterion.rs](https://github.com/bheisler/criterion.rs)).

- Benchmarks are located in [`crates/rbuilder/benches`](./crates/rbuilder/benches/). We'd love to add more meaningful benchmarks there!
- All PRs receive a [benchmark report like this](https://flashbots-rbuilder-ci-stats.s3.us-east-2.amazonaws.com/benchmark/3b22d52-f468712/report/index.html).
- You can run benchmarks with `make bench` and open the Criterion-generated report with `make bench-report-open`.
- Let us know about further improvement ideas and additional relevant benchmarks.

### Other executables

* `misc-relay-slot` shows info about winning bid for the block
* `debug-bench-machine` tests execution performance
* `debug-order-input` observe input of the bundles and transactions
* `debug-order-sim` observe simulation of the bundles and transactions
* `debug-slot-data-generation` shows new payload jobs coming from CL with attached data from relays.

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

None of this would have been possible without having a fast and efficient Ethereum node, so big shoutout to the [Reth](https://github.com/paradigmxyz/reth) team.


