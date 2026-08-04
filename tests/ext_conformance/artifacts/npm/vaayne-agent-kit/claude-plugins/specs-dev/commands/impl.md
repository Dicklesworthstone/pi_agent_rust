---
description: Implement feature from spec with iterative codex review and commits
allowed-tools: Read(*), Write(*), Edit(*), Glob(*), Grep(*), Task, TodoWrite, Bash
---

# Feature Implementation with Codex Review

You are executing a disciplined implementation workflow that keeps Codex in the loop and lands focused, validated commits.

## Quickstart Flow

| Phase                       | What you do                                                                                                 | Exit criteria                                                                                   |
| --------------------------- | ----------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| 1. Plan Analysis            | Open the provided session directory, read `plan.md` + `tasks.md`, understand scope and constraints.         | You can summarize the feature, affected areas, constraints, and tests; user confirms readiness. |
| 2. Task Breakdown           | Break work into small, independent tasks (1–3 files each), sync TodoWrite and `tasks.md`.                   | Ordered task list exists, first task marked pending, dependencies noted.                        |
| 3. Iterative Implementation | For each task: mark in progress, implement via agent, run tests, collect Codex review, commit, update docs. | All tasks complete, Codex feedback addressed, commits passing.                                  |
| 4. Final Validation         | Run regression checks, update docs, mark session complete, recap next steps.                                | Tests green, docs updated, user approves completion.                                            |

### Workflow Loop (Phase 3)

1. Update TodoWrite → current task `in_progress`.
2. Launch implementation agent (`task-implementer`) with explicit instructions.
3. Run required tests or checks.
4. Request Codex review (`codex-analyzer`) on the diff or files.
5. Apply feedback, rerun tests.
6. Commit code changes with `emoji + conventional` message.
7. Document implementation in `tasks.md` (files changed, approach, gotchas, commit hash).
8. Document deviations in `plan.md` (only if approach changed or new decisions made).
9. Mark task `done` in TodoWrite.
10. Move to the next task.

## Detailed Reference

### Phase 1 – Plan Analysis

**Goal:** Internalize the session plan before touching code.

**Steps:**

1. Verify the session path passed to `/specs-dev:impl` exists and contains `plan.md` and `tasks.md`.
2. Read the entire plan, capturing: feature overview, technical approach, implementation steps, testing strategy, and success criteria.
3. Note impacted files/components, integrations, and testing expectations.
4. Summarize the plan back to the user and confirm readiness to proceed.

**Checklist:**

- [ ] Session documents reviewed end-to-end.
- [ ] Key components/files identified.
- [ ] Constraints, risks, and acceptance criteria captured.
- [ ] User approval explicitly received before moving on.

### Phase 2 – Task Breakdown

**Goal:** Create actionable, incremental tasks mapped to the plan.

**Steps:**

1. Translate the plan’s implementation steps into granular tasks (1–3 files each, independently testable).
2. Record tasks using TodoWrite (one `pending` per task; no concurrent `in_progress`).
3. Mirror the same list in `tasks.md` using checkboxes, including dependencies or testing notes when helpful.
4. Highlight riskier tasks or external dependencies.

**Task Template:**

- Objective
- Files to touch
- Expected output
- Tests to run

**Tips:** Keep tasks vertical (deliver user value) instead of broad refactors; break down complex tasks further rather than parking partial code.

### Phase 3 – Iterative Implementation

Repeat the cycle for each task:

1. **Start** – Mark the TodoWrite item `in_progress`.
2. **Implement** – Use the Task tool (`task-implementer`) with a detailed prompt: objective, files, acceptance tests, patterns to follow.
3. **Validate** – Run unit/integration tests relevant to the change. Document commands you run.
4. **Codex Review** – Call `codex-analyzer` with the diff or affected files. Request severity-ranked findings covering bugs, security/performance issues, and regressions.
5. **Address Feedback** – Apply fixes, rerun tests, and if changes are significant, re-run the review.
6. **Commit** – Ensure a clean diff, then commit with emoji-prefixed Conventional Commit messages (e.g., `✨ feat: add onboarding API handler`). One logical change per commit.
7. **Document Implementation** – Update `tasks.md` for this task:
   - Check off the task checkbox
   - Add implementation notes: files changed, one-sentence approach summary, any gotchas discovered
   - Link the commit hash
8. **Document Deviations** (if applicable) – Update `plan.md` ONLY if:
   - Implementation approach deviated from the original plan
   - New architectural decision was made
   - Risk or constraint discovered that affects future work
   - Add entry under "Implementation Progress" or "Deviations" section with date, what changed, and why
9. **Close Task** – Set TodoWrite to `done`. Confirm `tasks.md` is updated and documentation is complete.

**Quality Gates per Task:**

- [ ] Tests covering the change are added/updated and passing.
- [ ] Codex review feedback implemented.
- [ ] No TODOs or commented-out code left behind.
- [ ] Commit message follows emoji + Conventional Commit format.
- [ ] `tasks.md` updated with implementation notes and commit hash.
- [ ] `plan.md` updated if implementation deviated from original plan or new decisions were made.

### Phase 4 – Final Validation

1. Run the agreed regression/acceptance suite (from `plan.md`).
2. Update `plan.md` with aggregate insights: overall testing results, final status, known issues, follow-up work, and next steps. Do not repeat per-task details already captured during Phase 3.
3. Verify `tasks.md` shows all tasks completed with their implementation notes.
4. Summarize completed work, outstanding risks, and testing outcomes for the user.
5. Confirm the session is ready for merge/release.

**Completion Checklist:**

- [ ] All tasks marked complete in both TodoWrite and `tasks.md`.
- [ ] Tests (unit/integration/lint) run and passing.
- [ ] No pending Codex feedback or unanswered comments.
- [ ] Session docs reflect final state and any next steps.

## Reference Material

### Agent & Tool Guidance

- **Implementation agent:** `task-implementer` (all implementation tasks), or `debugger` for troubleshooting failing tests.
- **Review agent:** `codex-analyzer`; include stack details, modules touched, and request severity-ranked output.
- **Todo management:** Use TodoWrite to keep one active task. Mirroring status in `tasks.md` avoids drift.

### Documentation Guidelines

**Information Architecture:**

- **TodoWrite:** Runtime task status only (in_progress/completed). Ephemeral, session-scoped. Cleared between sessions.
- **tasks.md:** Tactical implementation record. Persists across sessions. Contains task checklist with implementation notes.
- **plan.md:** Strategic feature record. Persists across sessions. Contains original plan plus deviations and aggregate insights.

**tasks.md Entry Format:**

Each completed task should follow this format:

```markdown
- [x] Task name
  - **Files:** `path/to/file1.ts`, `path/to/file2.ts`
  - **Approach:** Brief 1-sentence description of what was done
  - **Gotchas:** Any surprises, edge cases, or challenges discovered
  - **Commit:** {commit-hash}
```

**plan.md Updates:**

Update `plan.md` during implementation ONLY when:

- Implementation approach deviated from the original plan
- New architectural decision was made
- Risk or constraint emerged that affects future tasks
- External dependency or integration requirements changed

Add entries under an "Implementation Progress" or "Deviations" section:

```markdown
## Implementation Progress

**[Task Name]** (YYYY-MM-DD):

- **Original plan:** What was originally intended
- **Actual approach:** What was actually done
- **Reason:** Why the change was necessary
- **Impact:** How this affects future tasks (if any)
```

**Rule of Thumb:** If it only matters for THIS task → `tasks.md`. If it affects FUTURE tasks or understanding of the overall feature → `plan.md`.

### Best Practices

- Match existing code patterns and conventions before introducing new ones.
- Keep commits atomic and readable; amend only the latest commit if necessary.
- Keep code commits separate from documentation updates to maintain clean git history.
- Re-run impacted tests after each feedback iteration, not just once at the end.
- Document while context is fresh: update docs immediately after committing, not at the end of the day.

### Troubleshooting

- **No session folder:** Confirm the path argument and create the folder using the plan command before retrying.
- **Task too large:** Split it into smaller vertical slices; update TodoWrite and `tasks.md` accordingly.
- **Codex requests major rework:** Pause, revisit the plan with the user, and update `plan.md` before continuing.
- **Persistent test failures:** Switch to the `debugger` agent or request deeper Codex analysis targeting the failing module.
- **Messy commit history:** Use `git commit --amend` for the latest commit only. Avoid rewriting history beyond that without user approval.
- **Documentation debt accumulating:** If `tasks.md` hasn't been updated in 3+ completed tasks, pause and catch up before proceeding. Documentation loses value when written too long after implementation.

### Communication Tips

- Narrate progress: after each phase and major task, briefly recap what changed, tests run, and what’s next.
- Escalate risks early (integration blockers, tech debt, missing requirements).
- Encourage treating Codex review comments as blocking until resolved.

Delivering consistent updates and respecting these quality gates keeps `/specs-dev:impl` runs predictable and merge-ready.
