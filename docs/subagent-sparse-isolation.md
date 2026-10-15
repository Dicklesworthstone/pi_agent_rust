# Sparse-checkout subagent isolation

The native `subagent` worktree path can now start a child from a sparse source
checkout while giving that child a full tracked workspace. No new tool or
parallel snapshot engine is involved: ordinary, chained, parallel and corrective
runs reuse the existing captured isolation baseline.

```json
{
  "agent": "task",
  "task": "Inspect the package and its tracked dependencies, then implement the change.",
  "isolation": "worktree",
  "isoApply": "keep"
}
```

Use `keep` when the child may edit paths omitted from the parent checkout. It
retains the worktree and patch for deliberate review. The existing `apply` and
`drop` choices keep their normal acceptance and conflict handling; this feature
does not expand the parent's sparse checkout to make an excluded-file patch fit.
Edits to absent parent paths may fail existing patch-application checks and leave
the child available for inspection. No conflict is forced and no staged source
entry is replaced by the child's index.

## What the child receives

Present tracked files use their effective working contents, including unstaged
changes. Tracked files deliberately absent with Git's skip-worktree flag use
their original index mode and object, including staged content that differs from
HEAD. Present skip-marked files are refreshed normally rather than silently using
an old blob. Ordinary tracked deletions remain deletions. Nonignored untracked
files retain the existing inclusion behavior; ignored untracked files are not
newly included.

Missing skip-worktree entries are protected only in the private snapshot index.
The source index flags, staged entries and HEAD are not written. Paths remain
NUL-delimited, preserving tabs, newlines and Unix non-UTF-8 names. Symlink entries
keep their file type; an intermediate symlink while checking an excluded path is
an error, not permission to read through it.

Cone and non-cone sparse sources are supported, including sources whose index
contains compressed sparse-directory entries. Git expands those records for
capture; it is not asked to expand the source working directory. Initial child
checkout disables sparse filtering, then its own `config.worktree` records a
full checkout. The exact child configuration file is used, not a command that
could silently fall back to shared configuration. Linked sparse parent worktrees
keep their own index and local settings.

## Admission and limits

Sparse sources must already enable `extensions.worktreeConfig`, as modern
`git sparse-checkout` configurations do. Legacy sparse layouts with only shared
settings remain refused with `PI_ISO_SPARSE_UNSUPPORTED`. Pi does not migrate or
enable configuration on the user's behalf. Gitlinks/submodules remain unsupported
and fail before child launch; this is not recursive submodule materialization.

Index and flag record files retain the existing 32 MiB per-file limit. At most
65,536 absent skip-worktree paths are preserved. Malformed records, ambiguous
stages, duplicate paths, unexpanded directory entries and obstructed paths fail
rather than producing an incomplete baseline. Missing local Git objects and
checkout/configuration failures remain errors, not successful partial workspaces.

The capture verifies original source HEAD, staged entries, skip flags and the
continued absence of excluded paths. It also checks source checkout mode before
and after capture/checkout and checks the child's resulting full-checkout mode.
This is cooperative race detection, not a filesystem-wide transaction or an
atomic lock against external Git/configuration writers. An incomplete child may
exist after a later checkout/configuration error; the caller reports its path
for inspection and does not start the delegated process.

A full child can consume substantially more disk space than the sparse source.
The existing synchronous Git operations are not newly interruptible; original
subagent deadline/cancellation and result/writeback gates still apply around
them. Snapshot object creation and child branch/worktree registration remain
normal effects of isolation; "source unchanged" does not mean no Git objects or
worktree metadata are created.

## Validation

Twelve Rust regressions in `src/worktree_iso/snapshot/sparse/tests.rs` cover
preserved index-only content, present skip files, record/path admission, source
changes, full child checkout, cone/non-cone and compressed-index sources, linked
parents, legacy refusal and a public SubagentTool child-process/apply workflow.
The prior sparse-refusal test remains applicable to legacy shared configuration.

An independent real-Git matrix passed 96 checks over six scenarios: cone,
non-cone, compressed sparse index, linked compressed index, SHA-256 repository
and ordinary skip-worktree use. It checked content/mode preservation, clean
initial children, intended-only diffs, and unchanged source index/HEAD/config.
It does not execute the Rust code. The required `dsr quality --tool pi_agent_rust`
was unavailable in the implementation environment (command not found, exit 127).
Rust compilation, formatting, Clippy, these authored tests and cross-platform
acceptance remain unverified. No OMP-completeness claim follows from this feature.
