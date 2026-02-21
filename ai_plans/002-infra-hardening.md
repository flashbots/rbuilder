# vbuilder AI Infrastructure Hardening

## Executive Summary

A deep audit of vbuilder's AI developer infrastructure revealed 8 bugs, 14 gaps, and 8 nits across workflows, documentation, scripts, and slash commands. This plan addresses all bugs and the highest-impact gaps, organized for maximum parallel execution.

### Problem

The infrastructure built in Phase 1-5 has several issues that would cause incorrect behavior in production:

1. **Security review is broken** — the agent is told to read `.ai/safety.md` but the `Read` tool isn't in its allowed tools. It literally cannot do what it's told.
2. **Security review misses critical files** — `bidding_service_interface.rs` and `true_value_bidding_service.rs` (which control bid amounts) are outside the `bidding/**` glob and won't trigger security review.
3. **Documentation references wrong paths** — `.ai/safety.md` and `.ai/architecture.md` reference files that don't exist (`bidding/interfaces.rs`, `bidding/true_block_value_bidder.rs`, `bid_value_source/`).
4. **Playground script differs from CI** — missing `--features=""` on test command, causing potential "works in CI, fails locally."
5. **`make validate-config` validates nothing** — globs `config-*.toml` at repo root, but all config files are in subdirectories.
6. **Testing.md Tier 1 clippy command is incomplete** — missing `--workspace` flag.
7. **Pre-existing workflows overlap** — `claude.yaml` auto-reviews all PRs AND `security-review.yaml` reviews safety PRs, with no documentation of the intentional dual-review.
8. **Agent workflow has no bail-out** — compile loop can burn 60 minutes of API credits with no escape hatch.

### Proposed Solution

Fix all bugs, fill the highest-value gaps, and add the missing vibehouse-parity features (code review guide, PR guidelines, troubleshooting docs).

### Expected Outcomes

- Security review workflow correctly reads safety rules and catches all bidding-related changes
- All documentation paths reference files that actually exist
- Playground script matches CI behavior exactly
- Agent workflow has retry limits and failure guidance
- Pre-existing workflow overlap is documented and rationalized
- Agent has review/issue quality guidelines (vibehouse parity)

## Goals & Objectives

### Primary Goals
- Fix all 8 identified bugs so the infrastructure works correctly
- Fill the gaps that affect agent safety (security review paths, compile loop bail-out)

### Secondary Objectives
- Add code review and PR writing guidelines (vibehouse parity)
- Add playground troubleshooting guide
- Normalize status vocabulary and minor inconsistencies

## Solution Overview

### Key Components

1. **Security review fixes** — add `Read` tool, expand path filters to catch all bidding files
2. **Documentation fixes** — correct all wrong file paths in `.ai/safety.md` and `.ai/architecture.md`
3. **Script/CI parity** — align `playground-check.sh` with `checks.yaml`
4. **Agent hardening** — add retry limit, failure guidance, document workflow overlap
5. **Vibehouse parity** — add `.ai/code-review.md`, `.ai/pr-guidelines.md`, playground troubleshooting

### Architecture Diagram

No architecture changes. This is a documentation and configuration fix plan.

## Implementation Tasks

### CRITICAL IMPLEMENTATION RULES
1. Each task must be independently correct — no task should break if another is delayed
2. All file path references must be verified against the actual filesystem
3. Security-related fixes have highest priority

### Visual Dependency Tree

```
.github/workflows/
├── security-review.yaml         (Task #1: Fix allowed tools + path filters)
├── claude-issue-agent.yaml      (Task #3: Add retry limit + failure guidance)
├── claude.yaml                  (Task #5: Add comment noting dual-review intent)
└── claude-issues-ro.yaml        (Task #5: Document overlap with agent workflow)

.ai/
├── safety.md                    (Task #2: Fix wrong file paths)
├── architecture.md              (Task #2: Fix wrong file paths + trait locations)
├── testing.md                   (Task #4: Fix clippy command)
├── code-review.md               (Task #6: NEW — agent review guidelines)
└── pr-guidelines.md             (Task #7: NEW — PR/issue writing guidelines)

scripts/
└── playground-check.sh          (Task #4: Add --features="" to test command)

CLAUDE.md                        (Task #8: Add troubleshooting section + missing crates)

.claude/commands/
├── new-plan.md                  (Task #9: Fix status vocabulary)
└── review.md                    (Task #7: NEW — /review slash command)
```

### Execution Plan

#### Group A: Security Fixes (Execute all in parallel — HIGHEST PRIORITY)

- [x] **Task #1**: Fix security-review.yaml
  - File: `.github/workflows/security-review.yaml`
  - **Fix 1a**: Add `Read,Glob,Grep` to `--allowedTools` so the agent can read `.ai/safety.md`
  - **Fix 1b**: Expand path filters to catch ALL bidding-related files:
    ```yaml
    # Current (misses files outside bidding/ subdirectory):
    - "crates/rbuilder/src/live_builder/block_output/bidding/**"

    # Fixed (catch all bidding and bid-related files):
    - "crates/rbuilder/src/live_builder/block_output/bidding/**"
    - "crates/rbuilder/src/live_builder/block_output/*bidding*"
    - "crates/rbuilder/src/live_builder/block_output/*bid*"
    - "crates/rbuilder/src/live_builder/block_output/true_value*"
    ```
  - **Fix 1c**: Remove dead glob `relay_submit/**` (relay_submit is a file, not a directory)
  - Verify: the new globs would match `bidding_service_interface.rs`, `true_value_bidding_service.rs`, `bid_observer.rs`, `bid_observer_multiplexer.rs`

- [x] **Task #2**: Fix wrong file paths in .ai/safety.md and .ai/architecture.md
  - File: `.ai/safety.md` lines 79-81
    - Change `bidding/true_block_value_bidder.rs` → `true_value_bidding_service.rs` (at `block_output/` level)
    - Change `bidding/interfaces.rs` → `bidding_service_interface.rs` (at `block_output/` level)
    - Keep `bidding/sequential_sealer_bid_maker.rs` (correct)
  - File: `.ai/architecture.md` lines 106-114, Key Traits table:
    - `BiddingService` location: `block_output/bidding/interfaces.rs` → `block_output/bidding_service_interface.rs`
    - `SlotBidder` location: same fix
    - Remove `BidValueSource` / `BidValueObs` row (types don't exist in codebase)
    - Remove `BuilderSinkFactory` row if it doesn't exist as a trait (verify)
    - `SlotSource` row: note this is not a trait but a concrete type `MevBoostSlotDataGenerator`
  - File: `.ai/architecture.md` line 80:
    - Remove `NullBidValueSource` reference (type doesn't exist)
    - Replace with accurate description of current bid value behavior

#### Group B: Script/Doc Fixes (Execute all in parallel)

- [x] **Task #3**: Harden agent workflow
  - File: `.github/workflows/claude-issue-agent.yaml`
  - **Fix 3a**: Add retry guidance to Step 5 compiler loop:
    ```
    If cargo check or clippy fails 3 times on the same error, stop and open
    the PR as draft with a "needs help" label explaining the blocker.
    ```
  - **Fix 3b**: Add failure guidance for playground:
    ```
    If scripts/playground-check.sh fails, read the logs:
    - Check test.log first for test failures
    - Check playground.log for devnet health issues
    - Logs are in /tmp/playground-runs/<RUN_ID>/
    If integration tests fail twice, open the PR without integration test results
    and note this in the PR body.
    ```
  - **Fix 3c**: Remove `Bash(kill:*)` from allowed tools (unused, risky)
  - **Fix 3d**: Change `cancel-in-progress: false` to `cancel-in-progress: true`

- [x] **Task #4**: Fix playground script and testing.md
  - File: `scripts/playground-check.sh` line 109
    - Change: `PLAYGROUND=TRUE cargo test -p rbuilder --lib -- integration --test-threads=1`
    - To: `PLAYGROUND=TRUE cargo test --features="" -p rbuilder --lib -- integration --test-threads=1`
  - File: `.ai/testing.md` line 19
    - Change: `cargo clippy -- -D warnings`
    - To: `cargo clippy --workspace --features="" -- -D warnings`

- [x] **Task #5**: Document workflow overlap
  - File: `.github/workflows/claude.yaml`
    - Add a comment block at top explaining the three-workflow system:
      ```yaml
      # claude.yaml: General PR review + @claude interactive responses
      # security-review.yaml: Focused safety review (bidding, relay, blocklist, signing)
      # claude-issue-agent.yaml: Autonomous issue implementation
      #
      # PRs touching safety-critical paths get BOTH a general review (this file)
      # and a focused security review (security-review.yaml). This is intentional.
      ```
  - File: `.github/workflows/claude-issues-ro.yaml`
    - Add a comment noting:
      ```yaml
      # Read-only issue assistant. Responds to @claude mentions on issues.
      # Note: claude-issue-agent.yaml handles labeled issues (claude/agent-safe)
      # for autonomous implementation. This workflow is for Q&A only.
      ```

#### Group C: Vibehouse Parity (Execute all in parallel after Group A)

- [x] **Task #6**: Create .ai/code-review.md
  - File: `.ai/code-review.md` (NEW)
  - Modeled on vibehouse's CODE_REVIEW.md but adapted for MEV builder:
    - **Focus areas**: relay submission safety, blocklist enforcement, bidding correctness, key handling, async patterns, thread safety
    - **Review process**: limit to 3-5 key comments, focus on actionable issues
    - **Tone**: natural and conversational, not robotic (same guidance as vibehouse)
    - **Deep review techniques**: trace relay URL origins, check blocklist error paths, verify bid calculations
    - **Before approval checklist**: no mainnet URLs, blocklist intact, bidding unchanged unless intended, no key logging, tests present
    - **Anti-patterns**: over-engineering, unnecessary complexity, hiding errors
  - Update CLAUDE.md "Before You Start" table to include `| **Code review** | .ai/code-review.md |`

- [x] **Task #7**: Create .ai/pr-guidelines.md and /review command
  - File: `.ai/pr-guidelines.md` (NEW)
  - Modeled on vibehouse's ISSUES.md:
    - **PR structure**: Description section, test results, safety flags
    - **Issue structure**: Description, steps to resolve, code references with permalinks
    - **Writing style**: natural, concise, technical — avoid AI-sounding language
    - **Labels**: `agent`, `agent-safe`, `claude`, `needs-review`, `safety-critical`
    - **Anti-patterns**: vague descriptions, no code refs, premature solutions
  - File: `.claude/commands/review.md` (NEW)
    - Slash command that reads `.ai/code-review.md` then reviews current changes
    - Same pattern as vibehouse's `/review` command
  - Update CLAUDE.md "Before You Start" table to include `| **Creating issues/PRs** | .ai/pr-guidelines.md |`

- [x] **Task #8**: Add troubleshooting section and missing crates to CLAUDE.md
  - File: `CLAUDE.md`
  - **Fix 8a**: Add "Common Issues" subsection under Playground Testing:
    ```markdown
    ### Common Issues

    - **Build fails with missing libsqlite3-dev**: Install `sudo apt-get install libsqlite3-dev protobuf-compiler`
    - **Playground timeout**: Builder playground takes up to 60s to start. Check playground.log for errors. If reth fails to start, ensure --use-native-reth is set.
    - **MDBX lock errors in tests**: Too many concurrent tests. Always use --test-threads=10 for rbuilder crate.
    - **Integration tests pass locally but fail in CI (or vice versa)**: Ensure you use --features="" on both build and test commands. Compare your local command against checks.yaml.
    - **validate-config succeeds but config is wrong**: make validate-config only validates config-*.toml files at the repo root. Test configs in subdirectories are validated by their own tests.
    ```
  - **Fix 8b**: Add missing crates to test table:
    ```markdown
    ### test-relay
    cargo test -p test-relay

    ### bid-scraper
    cargo test -p bid-scraper

    ### sysperf
    cargo test -p sysperf
    ```

#### Group D: Nits (Execute in parallel, lowest priority)

- [x] **Task #9**: Normalize status vocabulary
  - File: `.claude/commands/new-plan.md` line 20
    - Change `**Status:** pending` to `**Status:** draft`
  - File: `PLAN.md` lines 9-13
    - Change all "complete" to "completed" (matching `docs/plans/README.md` vocabulary)
  - File: `docs/plans/README.md` line 14
    - Verify vocabulary includes: `draft | active | completed | abandoned` (already correct)

- [x] **Task #10**: Normalize checkout versions in workflows
  - Files: All `.github/workflows/*.yaml`
  - Standardize on `actions/checkout@v4` (the version used by the majority of workflows)
  - Affects: `claude.yaml` (currently v6), `claude-issues-ro.yaml` (currently v6), `checks.yaml` cargo-shear job (currently v6)

---

## Implementation Workflow

This plan file serves as the authoritative checklist for implementation. When implementing:

### Required Process
1. **Load Plan**: Read this entire plan file before starting
2. **Execute by Group**: Groups A-D can be started in order, but tasks within each group run in parallel
3. **Update checkboxes**: Mark `[x]` as each task completes
4. **Verify**: After all tasks, run a final cross-reference check: every file path mentioned in .ai/*.md and CLAUDE.md should point to a file that exists

### Critical Rules
- This plan file is the source of truth for progress
- Group A (security fixes) has highest priority — do these first
- Tasks within a group are independent and can be parallelized
- Verify all file paths against the actual filesystem before writing them into docs

### Progress Tracking
The checkboxes above represent the authoritative status of each task. Keep them updated as you work.
