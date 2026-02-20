# 000 - Origin Plan

**Status:** active
**Author:** halcyon
**Created:** 2026-02-20

## Summary

Transform vbuilder (a fork of flashbots/rbuilder) into an AI-maintained MEV block builder. This plan defines the full roadmap from establishing agent infrastructure through autonomous issue resolution, organized in five incremental phases.

## Motivation

rbuilder is a complex, performance-critical Rust codebase. Manual development is slow and context-heavy. By equipping AI agents with proper guardrails, testing harnesses, and CI workflows, we can:

- Accelerate development velocity on a large Rust codebase
- Maintain testing discipline through automated compiler and test loops
- Enable autonomous issue resolution with appropriate safety boundaries
- Keep upstream compatibility with flashbots/rbuilder

## Phase 1 - Foundation

Establish the documentation and tooling that agents need to work safely in this repo.

### CLAUDE.md

Root-level agent instructions file containing:

- **Build commands**: `cargo build`, `cargo clippy`, `cargo test` invocations with correct feature flags
- **Crate-specific test lookup table**: Maps each crate to its test command and expected behavior so agents know how to validate changes in any part of the repo
- **Safety rules**: Hard constraints agents must follow (never submit to mainnet relays, never disable blocklist checks, never modify bidding without review, etc.)
- **Repository structure overview**: Crate map and dependency relationships

### `.ai/` Directory

- `architecture.md` - High-level architecture doc covering the block building pipeline, order flow, and crate responsibilities
- `testing.md` - Testing strategy, how to run different test tiers, what needs playground vs unit tests
- `safety.md` - Expanded safety rules: relay submission guards, blocklist enforcement, bidding constraints, key management

### `.claude/commands/`

Custom slash commands for common agent workflows:

- `test` - Run the appropriate test suite for the current change
- `lint` - Run clippy with project-specific configuration
- `new-plan` - Create a new plan file from template
- `session-log` - Create a session log entry from template

## Phase 2 - CI/CD Upgrade

Upgrade GitHub Actions workflows for AI-agent integration.

### Claude Code Action

- Upgrade `claude.yaml` to use `claude-code-action@v1`
- Enable write permissions for issue-assigned triggers (agent picks up labeled issues)
- Configure appropriate model and token limits

### Security Review Workflow

- Add a dedicated security review workflow for PRs touching safety-critical paths
- Paths: relay submission, bidding logic, blocklist enforcement, key/signing code

### Branch Protection

- Require PR reviews for `develop` and `main`
- Require CI to pass before merge
- Block force pushes

## Phase 3 - Bot Deployment

Deploy the AI agent as an autonomous contributor.

### Issue-Driven Workflow

1. Issue is created and labeled (e.g., `claude`, `agent-safe`)
2. Agent is assigned (manually or via automation)
3. Agent creates a branch from `develop`
4. Agent implements the change, running compiler loop and tests
5. Agent opens a PR with description, test results, and session log
6. Human reviews and merges

### Guardrails

- **Max diff size**: Limit PR size to prevent runaway changes
- **Required labels**: Only work on issues with approved labels
- **Safety-critical path protection**: Changes to relay, bidding, blocklist, or signing code require human review regardless of label
- **No direct pushes**: All changes go through PRs

## Phase 4 - Testing Harness

Build the automated testing infrastructure that makes agent changes trustworthy.

### Compiler Loop

Standard agent workflow for any code change:

1. `cargo check` - Fast compilation check
2. `cargo clippy -- -D warnings` - Lint with warnings-as-errors
3. `cargo test` - Run relevant test suite
4. Iterate until all three pass

### Builder Playground Integration

- `scripts/playground-check.sh` - Script to run builder-playground integration tests
- Integrate with builder-playground-action for CI
- Smoke test: build a block with test orders, verify correctness

### New Integration Test Scenarios

- **Ordering correctness**: Verify transaction ordering respects gas price and bundle constraints
- **Bidding logic**: Test bid calculation and submission under various conditions
- **Cancellation handling**: Verify bundle and transaction cancellation propagates correctly

## Phase 5 - Self-Improvement

Enable the system to improve its own processes over time.

### Session Logging

- Agents write session logs after each work session (see [session log format](../sessions/README.md))
- Logs capture what worked, what didn't, and what to do next
- Enables continuity across sessions and agents

### Documentation Evolution

- Agents update `.ai/` docs when they discover outdated information
- CLAUDE.md test lookup table is kept current as crates change
- Plans are updated as phases complete

### Upstream Sync Strategy

- Regular sync with `flashbots/rbuilder` upstream
- Track divergence in a dedicated doc
- Prefer upstream changes unless vbuilder has intentional divergence

## Open Questions

These need decisions before or during implementation:

1. **Bot identity**: Should we use a dedicated bot account, or use `@claude` mentions on a shared account? A dedicated account provides cleaner audit trails.
2. **Mergify vs manual merge**: Should we use Mergify for auto-merging agent PRs that pass CI and review, or keep merging manual?
3. **Upstream sync frequency**: How often should we sync with flashbots/rbuilder? Weekly? Per-release? On-demand?
4. **Integration test cost**: Builder-playground tests may need 32-core runners. What's the acceptable CI cost per PR?
5. **Scope of autonomous changes**: Should agents be allowed to refactor without an issue? What about dependency updates?

## References

- [vibehouse](https://github.com/dapplion/vibehouse) - Documentation-driven AI development pattern
- [claude-code-action](https://github.com/anthropics/claude-code-action) - GitHub Action for Claude Code
- [builder-playground](https://github.com/flashbots/builder-playground) - Integration test environment
- [builder-playground-action](https://github.com/flashbots/builder-playground-action) - CI action for playground tests
- [flashbots/rbuilder](https://github.com/flashbots/rbuilder) - Upstream repository
