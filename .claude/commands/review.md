Review the current changes in this repository.

## Required Reading

**Before reviewing, read `.ai/code-review.md`** for vbuilder-specific safety requirements and review process.

## Steps

1. Read `.ai/code-review.md` for review guidelines
2. Read `.ai/safety.md` if the change touches safety-critical paths
3. Identify what changed:
   ```bash
   git diff --name-only HEAD
   git diff --stat HEAD
   ```
4. Review each changed file, focusing on:
   - Safety violations (relay URLs, blocklist bypass, bidding changes, key logging)
   - Correctness issues (bugs, race conditions, error handling)
   - Missing test coverage
5. Present 3-5 key findings (actionable issues only)

## Output

- Use natural, conversational language
- Provide specific file and line references
- Ask questions rather than making demands
- Only comment on issues that need attention — no praise

## After Review Discussion

If the developer corrects your feedback or you learn something new:

1. **Acknowledge and learn** — note what you got wrong
2. **Offer to update docs** — ask: "Should I update `.ai/code-review.md` with this lesson?"
3. **Format the lesson:**
   ```markdown
   ### Lesson: [Title]
   **Issue:** [What went wrong]
   **Feedback:** [What developer said]
   **Learning:** [What to do differently]
   ```
