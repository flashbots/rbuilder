# vbuilder Safety Rules

Constraints that exist to prevent financial losses, relay bans, or mainnet incidents. Safety-critical paths require human review before merging — the `.github/workflows/security-review.yaml` CI workflow auto-triggers on these paths.

## Safety-Critical Paths

PRs touching these files/directories will automatically trigger a focused security review:

- `crates/rbuilder/src/live_builder/block_output/relay_submit.rs` — relay submission
- `crates/rbuilder/src/live_builder/block_output/bidding/` — bidding logic
- `crates/rbuilder/src/live_builder/order_input/` — blocklist enforcement in order processing
- `crates/rbuilder-config/` — relay endpoint configuration
- Any file with `sign` or `key` in the path under `crates/rbuilder/src/`

## Rule 1: Relay Submission Guards

Relay URLs must come from config only — never hardcoded in source code.

```rust
// FORBIDDEN — hardcoded relay URL
const MAINNET_RELAY: &str = "https://relay.ultrasound.money";
let relay = RelayClient::new("https://boost-relay.flashbots.net");

// CORRECT — relay from validated config
let relay_urls = &config.relays; // loaded from TOML, validated on startup
```

NEVER:
- Add a hardcoded relay URL in any non-test file
- Bypass relay response validation (status codes, payload integrity checks)
- Submit a block without confirming the relay is in the approved config list

For tests: always use `crates/test-relay` (the in-process MEV-Boost test relay), never a real relay.

Check: all relay clients are constructed from `LiveBuilderConfig.relays` (loaded from TOML).

## Rule 2: Blocklist Enforcement

The blocklist prevents building blocks containing transactions from sanctioned addresses. Violating this can result in legal liability and relay bans.

NEVER:
- Comment out the blocklist check
- Add an early-return before blocklist validation
- Change blocklist loading to silently ignore errors

```rust
// FORBIDDEN
// blocklist_check(orders); // temporarily disabled for testing

// FORBIDDEN — silent failure
let blocklist = load_blocklist().unwrap_or_default(); // if file missing: empty blocklist = bypass

// CORRECT — loud failure
let blocklist = load_blocklist().expect("blocklist is required");
// or propagate the error up, causing the builder to refuse to start
```

If the blocklist file is missing or malformed: the builder must refuse to start, not silently disable the check.

## Rule 3: Bidding Constraints

The bidding pipeline is financially critical. Incorrect bids directly cause financial loss.

```
true_block_value = total MEV extractable from the block (financial ceiling)

bid < true_block_value  → we make profit
bid = true_block_value  → we make zero profit (all goes to proposer)
bid > true_block_value  → we lose money (subsidized)
```

NEVER (without explicit human sign-off):
- Change bid percentage calculations
- Modify the `true_block_value` calculation in `building/mod.rs`
- Add a subsidy path (bidding above `true_block_value`)
- Change `SlotBidder::send_bid` behavior

Files requiring extra scrutiny:
- `crates/rbuilder/src/live_builder/block_output/true_value_bidding_service.rs`
- `crates/rbuilder/src/live_builder/block_output/bidding_service_interface.rs`
- `crates/rbuilder/src/live_builder/block_output/bidding/sequential_sealer_bid_maker.rs`

Any PR changing bidding logic must include in the description: what the economics are, what the expected profit change is, and explicit sign-off from a human reviewer.

## Rule 4: Key and Signing Material

```rust
// FORBIDDEN at any log level
tracing::info!("private key: {:?}", private_key);
tracing::debug!("signing with: {}", hex::encode(key.to_bytes()));

// CORRECT — log the address, not the key
tracing::info!("signer address: {}", signer.address());
```

NEVER:
- Log private keys, mnemonics, raw signing material at any log level
- Serialize private keys in JSON responses, metrics, or health endpoints
- Commit config files containing real private keys

For tests needing a signing key:
```rust
// CORRECT — generate a throwaway key
let signer = LocalSigner::random();
```

Config files with real keys must be in `.gitignore` and never committed to git.

## Rule 5: Configuration Validation

Config defines the builder's operational envelope — relay endpoints, bidding parameters, blocklist path.

NEVER:
- Add a config field without a test in `rbuilder-config` tests
- Remove a config field without checking all callers
- Bypass `make validate-config` before a PR that changes config field definitions

```bash
# Must pass before any PR touching rbuilder-config/
make validate-config
```

The `validate-config` binary is the source of truth for what constitutes a valid config.

## What Triggers Human Review

Agents should proactively flag PRs that:
- Add or change how relay URLs are stored or used
- Change how orders are filtered before block inclusion
- Modify profit or bid calculations
- Add new signing operations or key handling
- Change error handling in config loading (especially blocklist or relay config)

The CI `security-review` workflow will also auto-trigger, but agents should not wait for CI to identify these — flag them at PR creation.

## Dev vs Production Boundaries

| Environment | Relay | Safe to run |
|-------------|-------|-------------|
| Builder playground | `test-relay` at `localhost:5555` | Yes — always |
| Staging | Test relay at known staging URL (from config) | Yes — with validated config |
| Mainnet | Real relay (Flashbots, Ultrasound, etc.) | Only via separate deployment, never from dev |

A staging or dry-run mode is NOT a substitute for testing against the test relay. Test relay produces real block proposals and bid traces — it gives full coverage without financial risk.
