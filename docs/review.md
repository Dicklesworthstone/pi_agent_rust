# Model code review

`pi review` and `/review` ask the selected model to inspect a Git diff and return
prioritized, located findings. The default command now uses the provider; the
older local string rules are available only through the explicit `--offline`
option. Reports identify the reviewer so the two modes are distinguishable.

## Run a review

```sh
pi review
pi review staged
pi review commit:HEAD
pi review branch:main
pi --model PROVIDER/MODEL review HEAD~3..HEAD --format json --fail-on P1
pi review staged --format markdown --out review.md
pi review --offline
```

Use any configured provider/model with the usual global model and credential
options. The standalone command uses the normal headless model resolver and
respects the project's trust decision. In either terminal interface, `/review`
uses the current session's provider and privacy settings:

```text
/review
/review staged
/review branch:main
/review --offline commit:HEAD
```

Press Esc or Ctrl-C to cancel an interactive review. The review owns isolated
provider requests, has no tools, and does not append its source snapshot or
reviewer conversation to the agent's model context. The terminal displays the
result as a report card.

## Select the changes

| Target | Changes reviewed |
| --- | --- |
| Omitted or `uncommitted` | Tracked working tree and index changes relative to HEAD, plus nonignored untracked files |
| `staged` | Index changes only |
| `commit:REF` | The changes introduced by that commit; merges are compared to their first parent |
| `branch:REF` | Committed changes from the merge base of REF and HEAD to HEAD |
| `A..B`, `A...B`, or another Git revision | The corresponding Git diff selection |

The snapshot is taken from the repository root even when Pi runs in a
subdirectory. Text file names containing spaces are supported. Git external
diff commands and text conversion drivers are disabled. The review uses text
hunks with 20 lines of surrounding context, without additional repository
exploration or cross-file agent tool calls.

Binary, submodule pointer, metadata-only, lockfile, and minified changes are outside text-review
coverage. Mixed reports list these in `skippedChanges` and explicitly limit the
verdict to analyzed text. If every change is outside coverage, the command fails
with `PI_REVIEW_NO_TEXT` instead of producing a clean verdict. Quoted or
control-character text paths and non-UTF-8 diff output are rejected.

## Findings and failure behavior

Each file receives an isolated model request. Findings must use P0–P3 severity,
a confidence between zero and one, an actionable explanation, and a new-side
line range inside a supplied hunk. The runtime validates the response and
attaches the exact file path itself. Malformed JSON, unsupported fields,
invented locations, tool requests, incomplete streams, and provider failures
fail the review. They do not trigger an offline scan or a successful verdict.

The report retains the `pi.review.v1` schema and adds `reviewer`,
`omittedFindings`, `failingFindings`, and `skippedChanges`. Findings are deduplicated
and ordered by severity and confidence. `--max-findings` limits display only:
every accepted finding still contributes to severity counts, the verdict, and
`--fail-on`. `--confidence-threshold` controls the fail-on confidence cutoff;
the existing BLOCK rule requires that cutoff for P0 and five percentage points
more for P1, capped at one. A fail-on match returns a nonzero exit status after
printing or saving the complete report.

The configured secrets policy screens all input files together before any
provider request, including bare echoes of credentials detected in another
file. Interactive requests also protect credentials already remembered by the
owning agent. Model-authored finding fields are screened before publication.
Block-mode privacy refusal stops the review. No report file is written when
provider execution, cancellation, or response validation fails.

## Resource limits

The model path accepts at most 32 text files, 192 KiB of diff input in total,
and 48 KiB per file. It runs at most three requests concurrently, allows 4,096
output tokens per request, and accepts at most 32 findings per file. The provider
phase has a shared 120-second deadline and a 64 KiB text-output cap per request.
Exceeding a limit fails explicitly; use a narrower target to review the remaining
changes. Git snapshot acquisition precedes the provider deadline.

An empty diff produces an empty, scoped report without calling the model.
`--offline` needs no provider credentials and remains a limited local heuristic
scan; its verdict is labeled accordingly.
