# vbuilder Plan

AI-maintained MEV block builder, forked from [flashbots/rbuilder](https://github.com/flashbots/rbuilder).

## Status

| Phase | Title                | Status  | Plan                                                  |
| ----- | -------------------- | ------- | ----------------------------------------------------- |
| 1     | Foundation           | pending | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-1---foundation) |
| 2     | CI/CD Upgrade        | pending | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-2---cicd-upgrade) |
| 3     | Bot Deployment       | pending | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-3---bot-deployment) |
| 4     | Testing Harness      | pending | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-4---testing-harness) |
| 5     | Self-Improvement     | pending | [000 - Origin Plan](docs/plans/000-origin-plan.md#phase-5---self-improvement) |

## Quick Links

- [Plans Index](docs/plans/README.md)
- [Session Logs](docs/sessions/README.md)
- Agent Instructions: TBD (Phase 1 deliverable: `CLAUDE.md`)
- [Upstream: flashbots/rbuilder](https://github.com/flashbots/rbuilder)

## Current Priorities

1. Create `CLAUDE.md` with build commands, crate test table, and safety rules
2. Set up `.ai/` directory with architecture, testing, and safety docs
3. Add `.claude/commands/` for agent workflows
4. Upgrade CI workflows for claude-code-action

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
├── CLAUDE.md                # Agent instructions (TBD)
├── docs/
│   ├── plans/               # Implementation plans
│   └── sessions/            # Agent session logs
├── .ai/                     # Agent context docs (TBD)
├── .claude/commands/        # Custom slash commands (TBD)
├── crates/                  # Rust workspace crates
├── scripts/                 # Build and test scripts
└── .github/workflows/       # CI/CD workflows
```
