# Session Logs

This directory contains session logs from AI-agent development sessions on vbuilder.

## File Naming

```
YYYY-MM-DD-HHMM.md
```

Use the session start time in UTC.

## Template

```markdown
# Session YYYY-MM-DD-HHMM

**Agent:** claude-code | claude-code-action
**Duration:** ~Xh
**Plan:** [NNN - Title](../plans/NNN-short-title.md)

## Context

Why this session was started and what it set out to accomplish.

## Work Completed

- Bullet list of concrete changes made
- Include commit SHAs where applicable (e.g. `abc1234`)
- Reference files changed

## Status Update

Current state of the relevant plan phase after this session.

## Next Steps

- What the next session should pick up
- Any blockers or open questions

## Handoff Notes

Anything the next agent (or human) needs to know that isn't captured above.
```

## Guidelines

- **Be concise.** Session logs are reference material, not narratives.
- **Link to plans.** Every session should reference the plan it advances.
- **Include commit SHAs.** Makes it easy to trace what changed and when.
- **Append-only.** Never edit a past session log. If corrections are needed, add a note in a new session.
