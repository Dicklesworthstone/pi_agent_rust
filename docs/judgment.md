# Native typed judgments

`pi::judgment::Judge` is an unstable library API for small, typed decisions.
Bind a `Provider` and `StreamOptions`, validate a `JudgmentRequest`, then await
`Judge::evaluate`. It makes one stateless, tool-free provider request. It does
not select a model, add ambient credentials, retry or fail over.

Requests contain exactly `state` and `questions`. State is nonempty text, an
object or an array. Questions are keyed by ID and have `type`, nonempty
`instructions`, and the following criteria:

- `bool`: optional `{ "true": "rubric", "false": "rubric" }`.
- `choice`: 2-16 labels mapped to rubric text or null.
- `score`: 2-16 ordered rubric strings, lowest first.

The limits are 16 questions, 128 KiB encoded input, 32 JSON levels, 65,536
values, 64-byte IDs/choice labels, and 4,096-byte rubric strings. Images,
unknown request fields and model-selection directives are not supported.

The model returns probabilities. Native validation requires all questions,
all choice labels and all score levels, with no duplicate or unknown keys.
Values must be finite numbers from zero to one. Distributions must sum to
one within 0.000001; accepted rounding is normalized. Missing answers,
booleans or numeric strings in place of probabilities, markdown wrappers,
truncated responses, tool calls and failed/absent terminal events are errors.
There is no partial-success or heuristic fallback verdict.

The result includes `schema: "pi.judgment.v1"`, `answers`, the producing
`provider`, `modelId`, qualified `model`, `api`, and reported terminal `usage`.
Catalog pricing is applied when supplied by the provider. Bool answers contain
`{type: "bool", bool: probability}`. Choice answers contain the highest-
probability `choice` (lexical ties), `probabilities` and `confidence`. Score
answers contain the weighted zero-based level index, probabilities keyed by
level index, and confidence. Confidence is one minus normalized entropy. These
are model estimates and distribution concentration, not calibrated accuracy.

The entire payload and system instruction pass the auxiliary privacy floor
before provider admission. Configured patterns extend it; block mode refuses.
An invocation may supply its live Agent projection internally. Redaction must
not change question IDs, answer types or choice labels. The original state and
rubric are never mutated. Provider failures use payload-free diagnostics.

A 60-second ceiling covers connection and response collection, within inherited
request cancellation/deadline authority. Responses are limited to 32 KiB and
4,096 requested output tokens (or the caller's lower token ceiling).

Usage is returned on successful calls; this API does not yet journal failed
attempts or update aggregate Agent session cost. It does not implement TypeSafe,
a role fallback chain, background batches, caching, or a retained workpool.
Eval helper integration is a separate change; merely exporting this module does
not yet expose a `judge` global in either kernel.

The module includes request/response validation and controlled provider-stream
unit fixtures. They are not live-model tests. The authoritative acceptance
command remains `dsr quality --tool pi_agent_rust`.
