# vbuilder AI Assistant Guide

This file provides guidance for AI assistants (Claude Code, Codex, etc.) working with vbuilder.

## vbuilder-Specific

vbuilder is a fork of [flashbots/rbuilder](https://github.com/flashbots/rbuilder), an AI-maintained MEV block builder.

- **Read `PLAN.md` first** — defines the work process, priorities, and phase status
- **Read `docs/plans/`** — implementation plans with detailed progress
- **Read `docs/sessions/`** — session logs for continuity across agents
- All work must be tracked in committed markdown docs
- Commit messages: lowercase, human-readable, no conventional commits
- Branch from `develop`, target `develop` for PRs

---

## Quick Reference

```bash
# Build (debug)
cargo build

# Build (release, reproducible on x86_64)
make build

# Lint
make lint

# Test (all unit tests)
make test

# Validate config files
make validate-config
```

## Testing — Run the Right Tests for What You Changed

Never run the full test suite routinely. Pick the command that matches the code you touched.

Note: always use `--test-threads=10` for the `rbuilder` crate — reth creates temporary MDBX databases per test and too many concurrent ones cause OOM.

### rbuilder-primitives

```bash
cargo test -p rbuilder-primitives
```

### rbuilder-utils

```bash
cargo test -p rbuilder-utils
```

### rbuilder-config

```bash
cargo test -p rbuilder-config
```

### eth-sparse-mpt

```bash
cargo test -p eth-sparse-mpt
```

### rbuilder (unit tests)

```bash
cargo test --features="" -p rbuilder -- --test-threads=10
```

### rbuilder (integration tests)

Requires builder-playground running. See the Playground Testing section below.

```bash
PLAYGROUND=TRUE cargo test -p rbuilder --lib -- integration --test-threads=1
```

### rbuilder-operator

```bash
cargo test -p rbuilder-operator
```

### rbuilder-rebalancer

```bash
cargo test -p rbuilder-rebalancer
```

### reth-rbuilder

```bash
cargo test -p reth-rbuilder
```

### test-relay

```bash
cargo test -p test-relay
```

### bid-scraper

```bash
cargo test -p bid-scraper
```

### sysperf

```bash
cargo test -p sysperf
```

### Before pushing

```bash
cargo test --features="" -- --test-threads=10
```

## Before You Start

Read the relevant guide for your task:

| Task | Read This First |
|------|-----------------|
| **Architecture questions** | `.ai/architecture.md` |
| **Testing strategy** | `.ai/testing.md` |
| **Safety constraints** | `.ai/safety.md` |
| **Code review** | `.ai/code-review.md` |
| **Creating issues/PRs** | `.ai/pr-guidelines.md` |
| **Upstream sync** | `.ai/upstream-sync.md` |

## Critical Rules (⛔ safety failures or financial loss)

### 1. Never Submit to Mainnet Relays

```rust
// FORBIDDEN — never hardcode relay URLs in tests or dev code
let relay_url = "https://relay.ultrasound.money"; // FORBIDDEN

// ALWAYS — relay URLs come from validated config only
let relay_url = &config.relays[0].url; // from loaded LiveBuilderConfig
```

Mainnet relay URLs must never appear in test code, scripts, or dev configs.
The `crates/test-relay` crate exists for integration testing — always use it.

### 2. Never Disable Blocklist Checks

The blocklist prevents building blocks that include transactions from sanctioned addresses.

NEVER:
- Comment out blocklist enforcement
- Add an early-return before blocklist validation
- Change blocklist loading to silently ignore errors (must fail loud — if the blocklist is missing or invalid, the builder must refuse to start)

### 3. Bidding Logic is Safety-Critical

Changes to `crates/rbuilder/src/live_builder/block_output/bidding/` REQUIRE human review.

NEVER:
- Change bid percentage or profit calculations without explicit sign-off
- Modify `true_block_value` calculation
- Add subsidy logic (bidding above `true_block_value`) without authorization

### 4. Never Log Private Keys or Signing Material

```rust
// FORBIDDEN
tracing::info!("private key: {:?}", private_key);

// ALLOWED — log the address, not the key
tracing::info!("signer address: {}", signer.address());
```

If tests need a signing key: generate a throwaway key with `LocalSigner::random()` — never use a real key.

## Important Rules (⚠️ bugs or correctness issues)

### 5. Thread Limit for Tests

```bash
# ALWAYS use --test-threads=10 for the rbuilder crate
cargo test -p rbuilder -- --test-threads=10
```

Reth creates temporary MDBX databases per test. Too many concurrent opens cause OOM.

### 6. Never Block Async

```rust
// NEVER block in async context
async fn handler() {
    expensive_computation(); // blocks the runtime
}

// ALWAYS spawn blocking
async fn handler() {
    tokio::task::spawn_blocking(|| expensive_computation()).await?;
}
```

### 7. TODOs Need Issues

All `TODO` comments must link to a GitHub issue.

## Playground Testing (Builder Playground)

### Quick Commands

```bash
# Run full devnet lifecycle (build + start playground + integration tests + teardown)
scripts/playground-check.sh

# Skip binary build (reuse existing debug binary)
scripts/playground-check.sh --no-build

# Leave playground running after test (for manual inspection)
scripts/playground-check.sh --no-teardown
```

### How the Devnet Works

- Starts: geth EL + beacon node + test MEV-boost relay + rbuilder
- rbuilder connects to reth database directly (`--use-native-reth` required — see `.ai/architecture.md`)
- Success = integration tests pass
- Logs go to `/tmp/playground-runs/<RUN_ID>/` with separate files (`build.log`, `playground.log`, `test.log`)
- 60-second timeout waiting for "All services are healthy"

### Bot Workflow

1. Make code change
2. Run `scripts/playground-check.sh`
3. On failure: read logs in `/tmp/playground-runs/<RUN_ID>/` — check `test.log` first, then `playground.log`
4. Fix the issue
5. Repeat

### Common Issues

- **Build fails with missing `libsqlite3-dev`**: Install native deps: `sudo apt-get install libsqlite3-dev protobuf-compiler`
- **Playground timeout (60s)**: Check `playground.log` for errors. If reth fails to start, ensure `--use-native-reth` is set in the playground command.
- **MDBX lock errors in tests**: Too many concurrent tests. Always use `--test-threads=10` for the rbuilder crate.
- **Tests pass locally but fail in CI (or vice versa)**: Ensure you use `--features=""` on both build and test commands. Compare your local command against `checks.yaml`.
- **`make validate-config` succeeds vacuously**: This target only validates `config-*.toml` files at the repo root. Test configs in subdirectories are validated by their own crate tests.
- **Integration test hangs**: The playground uses fixed ports (8545, 8645, 3500, 5555). Check that no other process is using these ports.

### Rules

- **NEVER** run integration tests against mainnet or production relays
- **NEVER** run `builder-playground start` directly — old processes accumulate and waste resources
- **ALWAYS** use `scripts/playground-check.sh` — it handles health polling, teardown, and log collection
- **ALWAYS** verify the relay endpoint in config is a test relay before running

## Project Structure

```
crates/
  rbuilder/               # Main block builder
    src/
      live_builder/       # LiveBuilder orchestrator
        order_input/      # OrderPool, RPC server, order replacement manager
        simulation/       # OrderSimulationPool (concurrent simulation threads)
        building/         # BlockBuildingPool, sink muxer
        block_output/     # Bidding, relay submission, bid value sources
        payload_events/   # MEV-Boost slot data (MevBoostSlotData)
      building/           # Building algorithms (greedy, etc.)
      mev_boost/          # MEV-Boost relay client
      provider/           # Reth provider integration
      roothash/           # State root computation (expensive — runs at sealing)
      backtest/           # Backtesting framework for algorithm comparison
  rbuilder-primitives/    # Core types (Order, Bundle, SimulatedOrder, bids)
  rbuilder-utils/         # Shared utilities
  rbuilder-config/        # Configuration parsing and validation
  rbuilder-operator/      # Operational tooling (deployment, monitoring)
  rbuilder-rebalancer/    # Bundle rebalancing between relays
  reth-rbuilder/          # Reth node integration (direct MDBX database access)
  eth-sparse-mpt/         # Sparse Merkle Patricia Trie for state computation
  test-relay/             # MEV-Boost test relay for integration testing
  bid-scraper/            # Historical relay bid data collection
  sysperf/                # System performance testing
```

See `.ai/architecture.md` for a detailed pipeline walkthrough.

## Maintaining These Docs

**These AI docs should evolve based on real interactions.**

### After Development Work

If you learn something about the codebase architecture or patterns:
- Ask: "Should I update `.ai/architecture.md` or `.ai/testing.md` with this?"
- Add to the relevant section

### After Safety Concerns

If you identify a new safety concern or pattern:
- Ask: "Should I update `.ai/safety.md` or the Critical Rules in CLAUDE.md?"

### Format for Lessons

```markdown
### Lesson: [Brief Title]

**Context:** [What task were you doing?]
**Issue:** [What went wrong or was corrected?]
**Learning:** [What to do differently next time]
```

### When NOT to Update

- Minor preference differences
- One-off edge cases unlikely to recur
- Already covered by existing documentation
