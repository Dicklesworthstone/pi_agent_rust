---
description: Collaborative planning workflow with Codex review before implementation
allowed-tools: Read(*), Write(*), Edit(*), Glob(*), Grep(*), Task, TodoWrite
---

# Feature Planning with Codex Review

You are facilitating a collaborative planning workflow that produces an approved implementation plan before any code is written.

## Quickstart Flow

| Phase                      | What you do                                                                                                      | Exit criteria                                                         |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| 1. Requirements Discussion | Interpret the request, ask targeted questions, refine scope with the user.                                       | User explicitly approves your summary ("OK", "ready", etc.).          |
| 2. Plan Creation           | Draft the implementation plan covering overview, technical approach, steps, testing, and considerations.         | Plan addresses every agreed requirement and is internally consistent. |
| 3. Codex Review            | Send the full plan to `codex-analyzer`, integrate its feedback, repeat if needed (≤3 rounds).                    | Codex confirms the plan is solid and feedback is incorporated.        |
| 4. Plan Documentation      | Save `plan.md` and `tasks.md` into `.agents/sessions/{YYYY-MM-DD-feature}/`, seed todos, and confirm next steps. | Session folder exists with up-to-date docs and TODOs.                 |

### Approval Gates

- **Gate A (end of Phase 1):** Summarize the requirements and ask, _"Do I understand correctly? Should I proceed to create the plan?"_ Stop until the user says yes.
- **Gate B (end of Phase 3):** Confirm with Codex feedback addressed and user satisfied before writing files.

### On-Run Checklist

- [ ] Requirements clarified, including scope boundaries and success criteria.
- [ ] Known constraints, integrations, and risks captured.
- [ ] Plan sections populated (overview → testing → considerations).
- [ ] Codex feedback captured, decisions documented.
- [ ] Session folder created with `plan.md` and `tasks.md`.

## Detailed Reference

### Phase 1 – Requirements Discussion

**Goal:** Reach a shared understanding of what to build before writing the plan.

**Steps:**

1. State your initial interpretation of the feature request.
2. Ask clarifying questions about functionality, user goals, constraints, success metrics, integrations, and out-of-scope items.
3. Iterate: reflect the user’s answers, tighten your understanding, and ask follow-ups where fuzzy.
4. Summarize the final requirements and run Approval Gate A.

**Guidelines:** Be concise but thorough, don’t assume missing details, and surface potential risks early.

### Phase 2 – Plan Creation

Produce a plan that is ready for implementation. Include:

1. **Overview** – Feature summary, goals, success criteria, and key requirements.
2. **Technical Approach** – Architecture, design decisions, tooling, component breakdown, data models, APIs, and integration notes.
3. **Implementation Steps** – Ordered tasks with dependencies, estimated effort, and risk callouts.
4. **Testing Strategy** – Unit/integration tests, edge cases, validation, and error handling.
5. **Considerations** – Security, performance, scalability, maintenance, documentation, and open questions.

Quality bar checklist:

- [ ] Every requirement from Phase 1 is addressed explicitly.
- [ ] Tasks are actionable and logically ordered.
- [ ] Rationale for key decisions is documented.
- [ ] Testing and edge cases are spelled out.
- [ ] Risks and mitigations are captured.

### Phase 3 – Codex Review & Refinement

1. Submit the full plan to the Task tool using the `codex-analyzer` agent (`codex --cd "{repo}" exec "agent codex-analyzer: …"`).
2. Request feedback on completeness, technical soundness, security/performance implications, and risk areas.
3. Integrate Codex feedback; clarify or adjust sections as needed.
4. Iterate (maximum of three passes) until Codex indicates the plan is comprehensive and ready.

**Best Practices:**

- Always send the entire plan, not a summary.
- Be explicit about the angle of review (security, performance, edge cases, etc.).
- Note Codex recommendations in the plan so decisions remain traceable.

### Phase 4 – Plan Documentation

1. Create the session directory at `.agents/sessions/{YYYY-MM-DD-feature-name}/`.
2. Save the finalized plan as `plan.md` and seed `tasks.md` with the implementation steps (checkbox list, owners/notes optional).
3. Confirm the session path with the user, summarize next steps, and remind them that `/specs-dev:impl` consumes this directory.

## Additional Guidance

### File Organization

- Sessions live under `.agents/sessions/`. Use YYYY-MM-DD and kebab-case feature names.
- `plan.md` and `tasks.md` stay authoritative; update them whenever requirements change.

### Communication Tips

- Keep the user in the loop—summaries after each major clarification help avoid rework.
- Surface uncertainties immediately; it’s cheaper to resolve them before plan creation.
- Encourage the user to treat Codex feedback as blocking until addressed.

### Troubleshooting

- **User keeps revising requirements:** Spend more time in Phase 1 capturing complete context; update the summary until the user signs off.
- **Codex feedback feels generic:** Provide sharper prompts outlining stack, modules, and risk areas.
- **Plan drifts high-level:** Add explicit file names, interface descriptions, data contracts, and test outlines to anchor the plan.
- **Session directory missing:** Ensure Phase 4 runs and paths are correct; recreate if necessary before invoking `/specs-dev:impl`.

### Tips for Excellent Plans

1. Patience in Phase 1 pays off—better questions reduce redo loops later.
2. Don’t rush the plan; specificity makes `/specs-dev:impl` straightforward.
3. Trust Codex feedback and document the adjustments you make.
4. Keep tasks bite-sized so future commits stay clean and reviewable.
