Create a new implementation plan from template.

## Steps

1. If the plan name and description were not provided in the command invocation, ask:
   - "What is the plan title?" (e.g., "Add competitive bidding service")
   - "One-sentence summary of what this plan accomplishes"

2. Find the next plan number:
   ```bash
   ls docs/plans/*.md | grep -oE '[0-9]+' | sort -n | tail -1
   ```
   Increment by 1 and zero-pad to 3 digits. If no numbered plans exist, start at `001`.

3. Create the file `docs/plans/NNN-short-title.md` (NNN = zero-padded number, short-title = lowercase-hyphenated) with this template:

   ```markdown
   # NNN - [Full Title]

   **Status:** draft
   **Author:** [agent name or human]
   **Created:** [today's date YYYY-MM-DD]

   ## Summary

   [One paragraph describing what this plan accomplishes and why.]

   ## Phase 1 - [Phase Name]

   [Description of what this phase does and why it's ordered first.]

   ### Deliverables

   - [ ] [Specific deliverable]
   - [ ] [Specific deliverable]

   ## Open Questions

   [Any decisions that need to be made before or during implementation.]

   ## References

   - [Link to relevant upstream issue, doc, or PR]
   ```

4. Add the plan to the status table in `PLAN.md`:
   ```markdown
   | N | [Title] | pending | [NNN - Title](docs/plans/NNN-short-title.md) |
   ```

5. Commit both files:
   ```bash
   git add docs/plans/NNN-short-title.md PLAN.md
   git commit -m "add plan NNN: short title"
   ```

6. Report: "Created docs/plans/NNN-short-title.md and updated PLAN.md"
