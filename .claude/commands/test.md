Run the appropriate test suite for the current change.

## Steps

1. Identify changed crates using `git diff --name-only HEAD` (or `git status` for unstaged changes)

2. Map changed files to crate test commands using this table:

   | Changed path | Test command |
   |-------------|-------------|
   | `crates/rbuilder-primitives/` | `cargo test -p rbuilder-primitives` |
   | `crates/rbuilder-utils/` | `cargo test -p rbuilder-utils` |
   | `crates/rbuilder-config/` | `cargo test -p rbuilder-config` |
   | `crates/eth-sparse-mpt/` | `cargo test -p eth-sparse-mpt` |
   | `crates/rbuilder/` | `cargo test --features="" -p rbuilder -- --test-threads=10` |
   | `crates/rbuilder-operator/` | `cargo test -p rbuilder-operator` |
   | `crates/rbuilder-rebalancer/` | `cargo test -p rbuilder-rebalancer` |
   | `crates/reth-rbuilder/` | `cargo test -p reth-rbuilder` |
   | `crates/test-relay/` | `cargo test -p test-relay` |

3. Run `cargo check` first (fast compile check). If it fails, report the error and stop.

4. Run the crate-specific test command(s) for everything that changed.

5. If the change touches any of these safety-critical paths, also recommend running `scripts/playground-check.sh`:
   - `crates/rbuilder/src/live_builder/block_output/relay_submit.rs`
   - `crates/rbuilder/src/live_builder/block_output/bidding/`
   - `crates/rbuilder/src/live_builder/order_input/`

6. Report results: pass/fail for each test command, with full output for any failures.

## Notes

- Always use `--test-threads=10` for `rbuilder` crate tests (reth MDBX memory limit)
- Integration tests (`PLAYGROUND=TRUE`) require builder-playground running — use `scripts/playground-check.sh` instead
- If multiple crates changed: run all their test commands
