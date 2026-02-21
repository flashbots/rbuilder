# vbuilder Testing Strategy

Testing tiers, exact commands per crate, and builder-playground integration guidance.

## Three Tiers

| Tier | Command | When to Use | Speed |
|------|---------|-------------|-------|
| 1. Compile check | `cargo check && cargo clippy -- -D warnings` | Every change — fastest feedback | ~10–30s |
| 2. Unit tests | `cargo test -p <crate> -- --test-threads=10` | After compile passes; for most changes | ~1–5 min |
| 3. Integration (playground) | `scripts/playground-check.sh` | Before PR; any relay/bidding/order change | ~5–10 min |

Use the lowest tier that covers the change. Don't run the full workspace test suite unless you changed something that touches many crates.

## Tier 1: Compile Check

```bash
cargo check                         # fast type-check, no codegen
cargo clippy --workspace --features="" -- -D warnings  # lint with warnings-as-errors
```

Run this constantly during development. If this fails, no need to run tests yet.

## Tier 2: Unit Tests

### Thread limit

Always use `--test-threads=10` when testing the `rbuilder` crate. Reth creates temporary MDBX databases per test; too many concurrent opens exhaust file descriptors and cause OOM:

```bash
# CORRECT
cargo test -p rbuilder -- --test-threads=10

# WRONG — will OOM on large test suites
cargo test -p rbuilder
```

The limit is not needed for crates that don't use reth dbs (`rbuilder-primitives`, `rbuilder-utils`, `rbuilder-config`, `eth-sparse-mpt`).

### Per-crate commands

```bash
# Core types and utilities (no thread limit needed)
cargo test -p rbuilder-primitives
cargo test -p rbuilder-utils
cargo test -p rbuilder-config
cargo test -p eth-sparse-mpt

# Main builder crate (thread limit required)
cargo test --features="" -p rbuilder -- --test-threads=10

# Operational crates
cargo test -p rbuilder-operator
cargo test -p rbuilder-rebalancer
cargo test -p reth-rbuilder

# Before pushing (full workspace, thread-limited)
cargo test --features="" -- --test-threads=10
```

### Running specific tests

```bash
# Single test by name
cargo test -p rbuilder test_name -- --test-threads=1

# Tests matching a pattern
cargo test -p rbuilder order_pool -- --test-threads=10

# With stdout (don't capture)
cargo test -p rbuilder -- --nocapture --test-threads=10

# Ignored tests (long-running or manually triggered)
cargo test -p rbuilder -- --ignored --test-threads=1
```

### Integration tests in the rbuilder crate

The `rbuilder` crate contains integration tests gated by `PLAYGROUND=TRUE`. These do NOT run in Tier 2 — they require Tier 3 setup:

```bash
# Only runs when builder-playground is already running
PLAYGROUND=TRUE cargo test -p rbuilder --lib -- integration --test-threads=1
```

## Tier 3: Integration Tests (Builder Playground)

### What builder-playground provides

A local devnet: geth EL + lighthouse CL + test MEV-boost relay + rbuilder wired together. The test relay runs at `localhost:5555`.

### Quick start

```bash
# Recommended: full automated lifecycle
scripts/playground-check.sh

# Skip cargo build (reuse existing debug binary)
scripts/playground-check.sh --no-build

# Leave devnet running for manual inspection
scripts/playground-check.sh --no-teardown
```

### Manual start (for debugging)

```bash
# 1. Build rbuilder
cargo build

# 2. Start playground in background
builder-playground start l1 --use-native-reth > /tmp/playground.log 2>&1 &

# 3. Wait for "All services are healthy" (up to 60s)
grep -m1 "All services are healthy" <(tail -f /tmp/playground.log)

# 4. Run integration tests
PLAYGROUND=TRUE cargo test -p rbuilder --lib -- integration --test-threads=1

# 5. Verify relay has mined blocks
curl http://localhost:5555/relay/v1/data/bidtraces/proposer_payload_delivered

# 6. Stop playground
kill %1
```

### Success criteria

- All tests in `rbuilder` integration module pass
- The test relay at `localhost:5555` returns bid traces (non-empty `proposer_payload_delivered`)
- No panics in playground logs

### Logs

`scripts/playground-check.sh` writes to `/tmp/playground-runs/<RUN_ID>/`:
- `build.log` — cargo build output
- `playground.log` — builder-playground startup and runtime output
- `test.log` — integration test output

On failure: check `test.log` first for test errors, then `playground.log` for devnet health issues.

## What Needs Which Tier

| Change | Minimum Tier |
|--------|-------------|
| Types, primitives, utilities | Tier 1 + Tier 2 (unit) |
| Config parsing | Tier 2 |
| Building algorithm | Tier 2 |
| Order simulation | Tier 2 |
| Root hash computation | Tier 2 |
| Relay submission path | Tier 2 + Tier 3 (integration) |
| Bidding logic | Tier 2 + Tier 3 |
| Order pool / RPC server | Tier 2 + Tier 3 |
| Blocklist enforcement | Tier 2 + Tier 3 |

## CI Test Matrix

What CI runs on every PR:

| Job | What it does |
|-----|-------------|
| `lint_and_test` | `make lint` + `make test` + `make validate-config` |
| `integration` | Downloads builder-playground v0.3.1, builds rbuilder, starts playground, runs integration tests |
| `cargo-shear` | Checks for unused dependencies |

Reproduce CI locally:

```bash
make lint && make test && make validate-config
```

## Backtest Framework

`crates/rbuilder/src/backtest/` — replay historical blocks using real order data from the bid-scraper.

This is **not a CI test tier** — it's used for benchmarking building algorithms against historical data. Run separately:

```bash
cargo run --bin backtest -- --config myconfig.toml
```
