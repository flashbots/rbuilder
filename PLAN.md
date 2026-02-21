# vbuilder Plan

AI-maintained MEV block builder, forked from [flashbots/rbuilder](https://github.com/flashbots/rbuilder).

## Status

| Phase | Title                | Status   | Plan                                                  |
| ----- | -------------------- | -------- | ----------------------------------------------------- |
| 1     | Foundation           | completed | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-1---foundation) |
| 2     | CI/CD Upgrade        | completed | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-2---cicd-upgrade) |
| 3     | Bot Deployment       | completed | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-3---bot-deployment) |
| 4     | Testing Harness      | completed | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-4---testing-harness) |
| 5     | Self-Improvement     | completed | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-5---self-improvement) |

## Quick Links

- [Plans Index](docs/plans/README.md)
- [Session Logs](docs/sessions/README.md)
- Agent Instructions: [CLAUDE.md](CLAUDE.md)
- [Upstream: flashbots/rbuilder](https://github.com/flashbots/rbuilder)

## Current Priorities

1. [001 - Integration Test Scenarios](docs/plans/001-integration-test-scenarios.md) — bundle inclusion, cancellation, bid value tests
2. Upstream sync — first sync with flashbots/rbuilder (see `.ai/upstream-sync.md`)
3. Agent-driven issues — label issues with `claude` or `agent-safe` to start autonomous development

## Decision Framework

When making implementation decisions, prefer:

- **Safety first** - Never compromise relay submission guards, blocklist checks, or key management
- **Upstream compatibility** - Minimize divergence from flashbots/rbuilder
- **AI-friendly** - Optimize for agent comprehension (explicit over implicit, tables over prose)
- **Incremental** - Each PR should be independently reviewable and revertable

## Repository Structure

```
vbuilder/
├── PLAN.md                  # This file - project dashboard
├── CLAUDE.md                # Agent instructions
├── docs/
│   ├── plans/               # Implementation plans
│   └── sessions/            # Agent session logs
├── .ai/                     # Agent context docs
│   ├── architecture.md      # LiveBuilder pipeline and crate map
│   ├── testing.md           # 3-tier testing strategy
│   ├── safety.md            # Safety-critical paths and rules
│   └── upstream-sync.md     # Upstream sync strategy and divergence tracking
├── .claude/commands/        # Custom slash commands (test, lint, new-plan, session-log)
├── crates/                  # Rust workspace crates
├── scripts/                 # Build and test scripts
└── .github/workflows/       # CI/CD workflows
```
