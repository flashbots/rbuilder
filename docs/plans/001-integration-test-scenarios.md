# 001 - Integration Test Scenarios

**Status:** active
**Author:** claude-code
**Created:** 2026-02-21

## Summary

Expand the builder-playground integration test suite with three new scenario categories: bundle ordering, bid value verification, and bundle cancellation. These tests run against the existing playground infrastructure in CI (`checks.yaml` integration job).

## Motivation

The current integration tests (`crates/rbuilder/src/integration/simple.rs`) cover single-transaction inclusion and blocklist enforcement. These are necessary but do not exercise core MEV builder behaviors: bundle handling, bid economics, and cancellation flows.

## Existing Infrastructure

- CI: `checks.yaml` integration job downloads `builder-playground v0.3.1`, starts devnet, runs tests
- Local: `scripts/playground-check.sh` wraps the same flow
- Test framework: `Playground` struct in `crates/rbuilder/src/integration/playground.rs`
- RPC: `eth_sendBundle`, `eth_cancelBundle` in `crates/rbuilder/src/live_builder/order_input/rpc_server.rs`
- Relay data: `ProposerPayloadDelivered` in `crates/rbuilder/src/mev_boost/mod.rs`
- All tests gated by `#[ignore_if_env_not_set("PLAYGROUND")]`

## Test Scenarios

### 1. Bundle Inclusion (ordering correctness)

**File:** `crates/rbuilder/src/integration/bundles.rs`
**Label:** `agent-safe`

Send a bundle containing a raw signed transaction via `eth_sendBundle` targeting the next block. Wait for inclusion and verify via receipt + `validate_block_built`.

Key points:
- Build a raw EIP-1559 tx, encode with `Encodable2718::encoded_2718()`
- Send as `eth_sendBundle` JSON-RPC to `rbuilder_rpc_url()` (localhost:8645)
- Bundle params: `{txs: ["0x..."], blockNumber: "0xN"}`
- Verify receipt exists and block was built by our builder

### 2. Bundle Cancellation

**File:** `crates/rbuilder/src/integration/bundles.rs`
**Label:** `agent-safe`

Send a bundle with a `replacementUuid`, then cancel it via `eth_cancelBundle` before the target block. Verify the transaction is NOT included.

Key points:
- Send bundle with `replacementUuid` field set
- Call `eth_cancelBundle` with `{replacementUuid, signingAddress}`
- Wait ~20s and assert no receipt (same pattern as blocklist test in `simple.rs`)

### 3. Bid Value Positive

**File:** `crates/rbuilder/src/integration/bundles.rs`
**Label:** `agent-safe`

After a successful bundle inclusion, call `validate_block_built` and assert `payload.value > U256::ZERO` — confirming the builder submitted a bid with positive value to the relay.

Key points:
- Extends the bundle inclusion test
- `validate_block_built` returns `ProposerPayloadDelivered` which has `value: U256`
- Assert `value > U256::ZERO`

## Implementation Notes

- All three tests go in a single new file `bundles.rs` with `mod bundles;` added to `integration/mod.rs`
- Helper functions: `build_raw_tx()`, `send_bundle()`, `cancel_bundle()` for JSON-RPC calls
- Must be implemented and tested in CI where builder-playground is available
- Use `--test-threads=1` (playground uses fixed ports)

## References

- [Origin Plan - Phase 4](000-origin-plan.md#phase-4---testing-harness)
- [Builder Playground](https://github.com/flashbots/builder-playground)
- Existing tests: `crates/rbuilder/src/integration/simple.rs`
