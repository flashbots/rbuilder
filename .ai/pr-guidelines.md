# PR and Issue Guidelines

Guidelines for creating well-structured GitHub PRs and issues for vbuilder.

## Pull Request Structure

```markdown
## Description

[What does this PR do? Why is it needed? 1-3 sentences, technical and specific.]

Closes #[issue-number]

## Test Results

[Paste the output of the test command you ran, or summarize pass/fail.]

## Safety

[If this PR touches safety-critical paths, include:]
⛔ touches safety-critical paths — requires careful human review

[Explain what safety-critical code changed and why.]
```

### Guidelines

- First line of description: what changed and why
- Link the issue with `Closes #N`
- Include test output — reviewers should see proof it works
- Flag safety-critical changes prominently
- Note large diffs (>500 lines) with explanation

### Commit Messages

Lowercase, human-readable, no conventional commit prefixes:

```
add order cancellation tracking to rpc server
fix blocklist loading to fail loud on missing file
update bidding service config defaults
```

## Issue Structure

### Start with Description

```markdown
## Description

[Problem and brief solution. Be technical and specific.]
[Context about current behavior.]
[Link to related issues, PRs, or specs.]
```

### Steps to Resolve (when applicable)

```markdown
## Steps to resolve

[Present options and considerations.]
[Don't be overly prescriptive.]
[Mention relevant constraints.]
```

### Code References

Use file paths with line numbers so agents can navigate directly:

```
See `crates/rbuilder/src/live_builder/block_output/relay_submit.rs:109`
```

## Writing Style

- **Natural and concise** — direct, technical, no filler
- **Honest about uncertainty** — don't guess, ask questions
- **Think about trade-offs** — present options, discuss pros/cons
- **Avoid AI-sounding language** — no "I'd be happy to", "Let me", "Certainly"

```
GOOD:
"The blocklist check currently uses unwrap_or_default which silently
degrades to an empty blocklist. This should propagate the error instead."

BAD:
"I've identified a potential issue with the blocklist implementation.
The current approach utilizes unwrap_or_default which could potentially
lead to a scenario where..."
```

## Labels

| Label | When to use |
|-------|-------------|
| `agent` | PR created by the autonomous agent |
| `agent-safe` | Issue safe for autonomous implementation |
| `claude` | Issue for autonomous implementation |
| `needs-help` | Agent couldn't complete — needs human guidance |
| `safety-critical` | Touches relay, bidding, blocklist, or signing code |

## Anti-Patterns

- Vague descriptions without specifics
- No code references when describing code
- Premature solutions without understanding the problem
- Claims without validation against the codebase
- Multi-paragraph explanations when one sentence suffices
