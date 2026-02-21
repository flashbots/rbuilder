# Upstream Sync Strategy

vbuilder is a fork of [flashbots/rbuilder](https://github.com/flashbots/rbuilder). This doc defines how to stay in sync.

## Principles

1. **Prefer upstream** — adopt upstream changes unless vbuilder has intentional divergence
2. **Minimize divergence** — keep vbuilder-specific changes isolated and well-documented
3. **Never auto-merge** — all upstream syncs go through PRs with human review

## Upstream Remote Setup

```bash
git remote add upstream https://github.com/flashbots/rbuilder.git
git fetch upstream
```

## Sync Process

### 1. Fetch and compare

```bash
git fetch upstream
git log --oneline develop..upstream/develop | head -30
```

### 2. Create sync branch

```bash
git checkout develop
git checkout -b sync/upstream-YYYY-MM-DD
git merge upstream/develop
```

### 3. Resolve conflicts

Conflicts will typically occur in:
- `CLAUDE.md`, `.ai/`, `.claude/`, `PLAN.md` — always keep ours (vbuilder-specific files)
- `.github/workflows/` — merge carefully, keep both upstream CI and vbuilder agent workflows
- `Cargo.toml` / `Cargo.lock` — take upstream, then verify `cargo check` passes
- `crates/` source code — case-by-case; prefer upstream unless vbuilder has intentional changes

### 4. Validate

```bash
cargo check
cargo clippy --workspace --features="" -- -D warnings
cargo test --features="" -- --test-threads=10
```

### 5. Open PR

Target `develop`. Title: `sync: upstream rbuilder YYYY-MM-DD`. Include:
- Upstream commit range merged
- Any conflicts resolved and how
- Test results

## Tracking Divergence

Maintain a list of intentional vbuilder divergences here. When resolving merge conflicts, check this list to know which side to keep.

### vbuilder-only files (always keep ours)

- `CLAUDE.md`
- `PLAN.md`
- `.ai/`
- `.claude/`
- `docs/plans/`
- `docs/sessions/`
- `scripts/playground-check.sh`
- `.github/workflows/claude-issue-agent.yaml`
- `.github/workflows/security-review.yaml`

### Intentional code divergences

_None yet. Add entries here as vbuilder diverges from upstream._

Format:
```
- **File**: `path/to/file.rs`
  **What**: Brief description of the divergence
  **Why**: Reason for diverging
  **Since**: Date or upstream commit hash
```

## Frequency

Sync when:
- Upstream cuts a release
- Upstream merges a change relevant to vbuilder's active work
- Divergence exceeds ~50 commits behind

There is no fixed schedule. The agent should not auto-sync — a human should trigger syncs.
