# Specs-Dev

**Plan, implement, and review features with spec-first automation.**

## Why Specs-Dev

Specs-Dev puts you in control of AI-assisted development with a disciplined, review-gated workflow:

- **Codex-reviewed plans** – Every plan is reviewed by Codex before you see it, catching architectural issues and edge cases early.
- **Codex-reviewed implementations** – Every code change gets AI review before commit, ensuring quality and consistency.
- **Approval gates at every stage** – No surprise implementations. You approve the plan before coding starts, and review changes before they're committed.
- **Human-in-the-loop control** – You decide when to proceed after seeing Codex-validated plans and code. No vibe coding chaos.
- **Specialized sub-agents** – Dedicated agents (task-implementer, codex-analyzer) provide better context management and focused expertise.
- **Structured command workflow** – `/plan`, `/impl`, and emoji-friendly conventional commits work together as a cohesive system.

## How It Differs from Standard AI Coding

| Standard AI Coding                    | Specs-Dev Workflow                                   |
| ------------------------------------- | ---------------------------------------------------- |
| AI starts coding immediately          | Plan first, then approve before any code             |
| You review code after it's written    | Codex reviews plans before you see them              |
| Fix issues after implementation       | Catch architectural issues during planning           |
| Context scattered across conversation | Focused sub-agents with clear responsibilities       |
| Ad-hoc commits                        | Structured tasks with Codex-reviewed implementations |

**The Specs-Dev Advantage**: Double validation at every stage (AI review + human approval) means fewer surprises, better quality, and you stay in control.

## Getting Started

1. **Add marketplace** – In Claude Code, run `/plugin` to add the repo as marketplace.
2. **Install** – Open `/plugin`, choose “Browse Plugins,” and install `specs-dev`. Enable it if it’s listed as disabled.
3. **Verify** – Run `/plugin` to confirm the `specs-dev:*` commands are registered.
4. **First run** – In any project, execute `/specs-dev:plan onboarding-flow`. Answer the guided questions, approve the summary, and let the command generate a session at `.agents/sessions/{date}-onboarding-flow/`.
5. **Review artifacts** – Open the session’s `plan.md` and `tasks.md`, adjust as needed, then step into implementation with `/specs-dev:impl .agents/sessions/{date}-onboarding-flow/`.

Requirements: Claude Code with Codex CLI access and Codex enabled.

For iterative development, keep this repository registered as a local marketplace and rebuild with `/plugin marketplace add  ./` whenever you update the commands or agents.

## Directory Overview

- `agents/` – Task agents that wrap Codex behaviors (e.g., `codex-analyzer`).
- `commands/` – Markdown specs that power the `/spec:*` commands.
- `hooks/` – Optional automation hooks (empty placeholder today).

## Workflow at a Glance

**Planning Phase** (with approval gates):

1. `/specs-dev:plan {feature}` – Gather requirements through guided questions
2. **→ Codex reviews plan** – Codex analyzes the plan for issues, edge cases, and improvements
3. **→ You approve** – Review the Codex-validated plan before any code is written
4. Session folder created with `plan.md` and `tasks.md`

**Implementation Phase** (with review loops):

1. `/specs-dev:impl {session-folder}` – Implements tasks one by one using specialized sub-agents
2. For each task:
   - Task-implementer agent writes focused code changes (1-3 files)
   - **→ Codex reviews implementation** – Checks for bugs, security issues, performance problems
   - **→ Fixes applied if needed** – Address feedback before proceeding
   - **→ Commit created** – Clean, emoji-friendly conventional commit
3. Iterate until all tasks complete

**Result**: Every line of code goes through AI review before commit, and you approve major decisions before implementation starts.

## Tips for Smooth Runs

- Keep implementation tasks scoped to 1–3 files so each commit stays focused.
- Update `plan.md` and `tasks.md` whenever requirements shift; the commands expect them to be current.
- Treat Codex feedback as a blocking review before merging any change set.
- Confirm the session docs are accurate before starting `/specs-dev:impl` to avoid redo loops.

Built for Claude Code · Powered by Codex
