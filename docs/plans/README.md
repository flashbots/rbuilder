# Plans

This directory contains implementation plans for vbuilder development.

## Format

Each plan is a Markdown file named `NNN-short-title.md` where `NNN` is a 3-digit zero-padded sequence number. Plans are numbered in creation order starting from `000`.

### Required Sections

Every plan should include a frontmatter block and the following sections:

- **Status** (`draft` | `active` | `completed` | `abandoned`), **Author**, **Created** date
- **Summary**: One-paragraph description of what this plan covers
- **Motivation**: Why this work is needed
- **Phases / Steps**: Concrete implementation steps
- **Open Questions**: Unresolved decisions (remove or move to a decisions section as they are resolved)
- **References**: Links to relevant issues, PRs, external docs

### Creating a Plan

1. Find the next available number: `ls docs/plans/*.md | tail -1`
2. Create `NNN-short-title.md` using the format above
3. Add an entry to the index table below
4. Link from `PLAN.md` if it affects the project roadmap

## Index

| #   | Title                              | Status | Author  | Created    |
| --- | ---------------------------------- | ------ | ------- | ---------- |
| 000 | [Origin Plan](000-origin-plan.md)  | active | halcyon | 2026-02-20 |
