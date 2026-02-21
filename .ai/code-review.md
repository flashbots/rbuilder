# vbuilder Code Review Guidelines

Code review guidelines for AI agents and human reviewers working on vbuilder.

## Core Principles

- **Safety** over cleverness — this is financial infrastructure
- **Correctness** through proper error handling and testing
- **Clarity** through good naming and documentation
- **Upstream compatibility** — minimize divergence from flashbots/rbuilder

## Focus Areas

### 1. Safety-Critical Paths

Extra scrutiny required for:
- `block_output/relay_submit.rs` — relay URLs come from config only, never hardcoded
- `block_output/true_value_bidding_service.rs` — bid calculation correctness
- `block_output/bidding_service_interface.rs` — bidding trait contracts
- `block_output/bidding/` — bid maker logic
- `order_input/` — blocklist enforcement must never be bypassed
- `rbuilder-config/` — config validation for relays and blocklist

Read `.ai/safety.md` before reviewing changes to these paths.

### 2. Relay Submission

```rust
// RED FLAG — hardcoded relay URL
let relay = RelayClient::new("https://boost-relay.flashbots.net");

// CORRECT — from validated config
let relay = RelayClient::from_url(config.relays[0].url.clone(), ...);
```

Check:
- Are relay URLs sourced exclusively from config?
- Is relay response validation intact?
- Are test relays used in tests (not real relays)?

### 3. Blocklist Enforcement

```rust
// RED FLAG — silent failure
let blocklist = load_blocklist().unwrap_or_default();

// CORRECT — loud failure
let blocklist = load_blocklist()?;
```

Check:
- Is blocklist loading still fail-loud?
- Are there any early-returns before blocklist validation?
- Are both sender and recipient checked?

### 4. Bidding Correctness

Check:
- Does `true_block_value` calculation change? If so, explain the economics.
- Is there any path where bid > true_block_value (subsidy)?
- Are bid percentage calculations correct?

### 5. Key and Signing Material

Check:
- No private keys logged at any level (including debug/trace)
- No keys serialized in JSON responses, metrics, or health endpoints
- Test keys are throwaway (`LocalSigner::random()`), not real

### 6. General Rust Quality

- No `.unwrap()` or `.expect()` in production paths (OK in tests and startup)
- No blocking in async context — use `spawn_blocking`
- Thread-safe access patterns (no data races)
- Error context provided (use `eyre`, `anyhow`, or `.context()`)

## Review Process

### Limit to 3-5 Key Comments

Prioritize:
1. Safety violations (relay, blocklist, bidding, keys)
2. Correctness issues (bugs, race conditions, error handling)
3. Missing test coverage for non-trivial changes
4. Complex logic needing documentation

Don't comment on:
- Minor style issues caught by `cargo fmt` or `cargo clippy`
- Performance optimizations unless they're clearly needed
- Praise or positive feedback — only actionable issues

### Tone

Natural and conversational, not robotic.

```
GOOD:
"Should we avoid unwrap() here? This gets called in the relay submission
path and a panic would drop the block."

BAD:
"This violates coding standards which strictly prohibit runtime panics
in production code paths. Please refactor to use the ? operator."
```

### Verify Before Commenting

- If CI passes, trust it — types and imports exist
- Check the full diff, not just the visible parts
- Ask for clarification rather than asserting things are missing
- Don't guess — read the code

## Deep Review Techniques

### Trace Relay URL Origins

For any change touching relay logic:
1. Where does the relay URL come from? Follow it back to config loading.
2. Is it validated before use?
3. Could a malformed config inject an unintended URL?

### Check Blocklist Error Paths

For any change near order filtering:
1. What happens if the blocklist file is missing?
2. What happens if the blocklist is empty?
3. Is there any code path that skips the check?

### Verify Bid Economics

For any change to bidding:
1. What is the expected profit/loss under this change?
2. Could a bug cause bid > true_block_value?
3. Is the change compatible with the current `TrueBlockValueBiddingService`?

### Look for Incomplete Migrations

When a PR changes a pattern across the codebase:
1. Search for the old pattern — all occurrences updated?
2. Check test files — often lag behind implementation

## Before Approval Checklist

- [ ] No mainnet relay URLs in test or dev code
- [ ] Blocklist enforcement intact
- [ ] Bidding logic unchanged (or explicitly reviewed and approved)
- [ ] No private keys logged or serialized
- [ ] Tests present for non-trivial changes
- [ ] `--test-threads=10` used for rbuilder crate tests
- [ ] No blocking in async context
- [ ] TODOs linked to GitHub issues
