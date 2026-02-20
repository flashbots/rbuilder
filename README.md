# vbuilder

An AI-maintained Ethereum MEV-Boost block builder, forked from [flashbots/rbuilder](https://github.com/flashbots/rbuilder).

vbuilder is the "vibe block builder" — an experiment in using AI agents to develop and maintain a production-grade block builder. The codebase tracks upstream rbuilder and extends it with AI-driven development infrastructure, automated testing harnesses, and documentation-first workflows.

> **Upstream**: vbuilder regularly syncs with [flashbots/rbuilder](https://github.com/flashbots/rbuilder). See the [Origin Plan](docs/plans/000-origin-plan.md) for the sync strategy.

## Project Status

See [`PLAN.md`](PLAN.md) for the full roadmap and current priorities.

| Phase | Title            | Status  |
| ----- | ---------------- | ------- |
| 1     | Foundation       | pending |
| 2     | CI/CD Upgrade    | pending |
| 3     | Bot Deployment   | pending |
| 4     | Testing Harness  | pending |
| 5     | Self-Improvement | pending |

## Features

Inherited from rbuilder:

- **Multiple algorithms**: Pluggable block building algorithms. The included algorithm sorts orders by effective gas price or total profit. See [ordering_builder.rs](crates/rbuilder/src/building/builders/ordering_builder.rs)
- **Backtesting**: Quick backtesting on mempool transactions and historical data
- **Bundle merging**: Bundles targeting already-included transactions can be dropped via `reverting_tx_hashes`
- **Smart nonce management**: Identifies and handles nonce dependencies between bundles and transactions
- **Built on [Reth](https://github.com/paradigmxyz/reth/)**: Fast, efficient Ethereum execution client in Rust
- **Reproducible builds**

## Running vbuilder

vbuilder can run in two modes:

### Backtesting

Build blocks against historical data using [mempool-dumpster](https://mempool-dumpster.flashbots.net/index.html) for historical mempool transactions. Plug in historical bundles for testing, and compare local block building against what actually landed on-chain. Results are cached in a local SQLite database for rapid iteration.

For details: [Noob Guide for Backtesting](https://github.com/flashbots/rbuilder/wiki/Noob-Guide-for-Backtesting)

### Live

Requirements:
- Reth node for state (`reth_datadir`) with IPC interface for mempool tx subscription (`el_node_ipc_path`)
- Reth configured to flush every block: `--engine.persistence-threshold "0" --engine.memory-block-buffer-target "0"`
- CL node triggering new payload events on every slot
- Bundle source sending `eth_sendBundle`, `mev_sendBundle`, `eth_sendRawTransaction` as JSON-RPC (`jsonrpc_server_port`)
- Relays to submit to (`relays`)

Sample Lighthouse config:
```bash
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

Sample Reth config:
```bash
reth node \
    --datadir /mnt/md0/rethdata \
    --authrpc.jwtsecret /secrets/jwt.hex \
    --authrpc.addr 127.0.0.1 \
    --authrpc.port 8551 \
    --http \
    --ws \
    --ws.addr 127.0.0.1 \
    --ws.port 8545 \
    --rpc-max-connections 429496729 \
    --http.api trace,web3,eth,debug \
    --ws.api trace,web3,eth,debug \
    --engine.persistence-threshold "0" --engine.memory-block-buffer-target "0"
```

Optional:
- Block processor API as a sink for submitted blocks (`blocks_processor_url`)
- Prometheus / Grafana for metrics (served on `telemetry_port` + `/debug/metrics/prometheus`)
- Trace recording (`tracing_path`, served on `telemetry_port` + `/debug/tracing/{start,stop}`)

Running:
1. Prepare config based on [`config-live-example.toml`](./examples/config/rbuilder/config-live-example.toml)
2. Run `rbuilder run PATH_TO_CONFIG_FILE`

See [`CONFIG.md`](./docs/CONFIG.md) for config field details.

**Warning**: Be aware of potential [reorg losses](./docs/REORG_LOSSES.md) before running a builder in production.

### End-to-End Local Testing

Use [builder-playground](https://github.com/flashbots/builder-playground) to deploy a fully functional local setup (Lighthouse + Reth + MEV-Boost-Relay):

```bash
git clone git@github.com:flashbots/builder-playground.git
cd builder-playground
go run main.go
```

Update [`config-playground.toml`](./examples/config/rbuilder/config-playground.toml) with fully-qualified paths:
```bash
sed -i "s|\$HOME|$HOME|g" ./examples/config/rbuilder/config-playground.toml
```

Run vbuilder:
```bash
cargo run --bin rbuilder run ./examples/config/rbuilder/config-playground.toml
```

Query the local relay:
```bash
curl http://localhost:5555/relay/v1/data/bidtraces/proposer_payload_delivered
```

### Testing with a Fake Relay

The `test-relay` binary implements the MEV-Boost Relay API for testing without submitting to production relays. It validates blocks and compares profits between builders.

```bash
./test-relay \
    --relay "https://boost-relay-hoodi.flashbots.net" \
    --validation-url "http://localhost:8545" \
    --cl-clients "http://localhost:5052"
```

Configure rbuilder to use it:
```toml
[[relays]]
name = "flashbots-test"
url = "http://localhost:80"
priority = 0
```

### Benchmarking

Based on [Criterion.rs](https://github.com/bheisler/criterion.rs). Benchmarks live in [`crates/rbuilder/benches`](./crates/rbuilder/benches/).

```bash
make bench
make bench-report-open
```

### Reproducible Builds

Set `SOURCE_DATE_EPOCH` for reproducible output:
```bash
export SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)
cargo build --release
```

## Binaries

| Binary                      | Description                                                  |
|-----------------------------|--------------------------------------------------------------|
| `rbuilder`                  | Live block builder                                           |
| `backtest-build-block`      | Backtest a single block                                      |
| `backtest-build-range`      | Backtest a range of blocks                                   |
| `backtest-fetch`            | Download backtesting data                                    |
| `dummy-builder`             | Sample builder showing custom `BlockBuildingSink` and algorithm |
| `misc-relays-slot`          | Show winning bid info for a block                            |
| `debug-bench-machine`       | Test execution performance                                   |
| `debug-order-input`         | Observe bundle and transaction input                         |
| `debug-order-sim`           | Observe bundle and transaction simulation                    |
| `debug-slot-data-generator` | Show new payload jobs from CL with relay data                |
| `test-relay`                | Test MEV-Boost relay for block validation                    |

## Contributing

```bash
git clone git@github.com:dmarzzz/vbuilder.git
cd vbuilder

make lint    # Run linter
make test    # Run tests
make bench   # Run benchmarks
```

See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for guidelines.

## Security

See [`SECURITY.md`](./SECURITY.md)

## Acknowledgements

- [flashbots/rbuilder](https://github.com/flashbots/rbuilder) — the upstream block builder this fork is based on
- [Reth](https://github.com/paradigmxyz/reth) — the Ethereum execution client powering the builder
- [vibehouse](https://github.com/dapplion/vibehouse) — inspiration for the documentation-driven AI development approach
