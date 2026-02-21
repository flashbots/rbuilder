Create a session log entry from template.

## Steps

1. Get the current UTC timestamp:
   ```bash
   date -u +%Y-%m-%d-%H%M
   ```

2. Create `docs/sessions/YYYY-MM-DD-HHMM.md` using the template from `docs/sessions/README.md`:

   ```markdown
   # Session YYYY-MM-DD-HHMM

   **Agent:** claude-code
   **Duration:** ~Xh
   **Plan:** [NNN - Title](../plans/NNN-short-title.md)

   ## Context

   [Why this session was started and what it set out to accomplish.]

   ## Work Completed

   - [Bullet list of concrete changes]
   - [Include commit SHAs: e.g. `abc1234`]
   - [Reference files changed]

   ## Status Update

   [Current state of the plan phase after this session. Which tasks are complete, which are in progress.]

   ## Next Steps

   - [What the next session should pick up]
   - [Any blockers or open questions]

   ## Handoff Notes

   [Anything the next agent or human needs to know that isn't captured above.]
   ```

3. Fill in all sections based on the current session's actual work:
   - Work Completed: bullet list with commit SHAs where available
   - Status Update: which plan phase this session advanced
   - Next Steps: what remains and any blockers

4. Commit:
   ```bash
   git add docs/sessions/YYYY-MM-DD-HHMM.md
   git commit -m "session log YYYY-MM-DD-HHMM"
   ```

5. Report: "Session log written to docs/sessions/YYYY-MM-DD-HHMM.md"

## Notes

- Be concise — session logs are reference material, not narratives
- Always reference the plan being advanced
- Append-only: never edit past session logs
- If corrections are needed, add a note in a new session log
