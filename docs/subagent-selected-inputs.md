# Data handoff from a selected workflow branch

Use `fromSelected` to pass the result of one mutually exclusive branch into a
common continuation. This complements `join: selected`: the join controls
whether the node may run; the input chooses its one accepted data source.

```json
{
  "workflow": [
    {
      "id": "choose",
      "call": {
        "agent": "scout", "task": "Choose fast or thorough review and return take.",
        "outputSchema": {
          "type": "object", "required": ["take"],
          "properties": {"take": {"enum": ["fast", "thorough"]}}
        }
      }
    },
    {
      "id": "fast", "needs": ["choose"],
      "when": {"from": "choose", "pointer": "/take", "equals": "fast"},
      "call": {"agent": "reviewer", "task": "Perform the scoped review. Return findings.",
        "outputSchema": {"type": "object", "required": ["findings"]}}
    },
    {
      "id": "thorough", "needs": ["choose"],
      "when": {"from": "choose", "pointer": "/take", "equals": "thorough"},
      "call": {"agent": "reviewer", "task": "Perform the full review. Return findings.",
        "outputSchema": {"type": "object", "required": ["findings"]}}
    },
    {
      "id": "continue", "needs": ["fast", "thorough"], "join": "selected",
      "inputs": {"review": {"fromSelected": ["fast", "thorough"], "pointer": "/findings"}},
      "call": {"agent": "task", "task": "Verify the selected review findings and address confirmed defects."}
    }
  ]
}
```

Each input reference has `pointer` and **exactly one** of `from` or
`fromSelected`. Existing `from` inputs still require that exact source; they do
not acquire fallback behavior. `fromSelected` requires 2–31 distinct IDs, all
explicit direct dependencies, and an explicit `join: selected` on the consuming
node. The whole graph checks the effective output schemas of every candidate
before launching any node, including candidates that may never be selected.

## Selection rules

Exactly one candidate must have host state `accepted`; every other candidate
must be intentionally `not_selected`. Two accepted candidates produce
`selected_input_ambiguous`, never a first/fastest/last winner. A failed, skipped,
pending or unknown candidate produces `selected_input_dependency_failed`;
another success does not hide it. The existing join admission normally rejects
such dependencies even before input selection runs.

A selected result still needs schema-validated data. Missing or unavailable
collection data is `dependency_data_unavailable`, not permission to parse its
prose or try another candidate. The JSON pointer applies only to the selected
value. A missing field is `dependency_input_missing`; null, false, zero and
empty strings remain real values. Complete validated fan-out arrays work as
sources under their existing collection limits.

If all dependencies are intentionally unselected, the existing join leaves
the continuation unselected too, without evaluating its inputs or launching a
child. If a join otherwise runs but this particular candidate set has no
selected source, the input returns `selected_input_unavailable`.

Selection is deterministic over settled records and does not alter graph
edges, wave barriers, workspace acceptance, original deadlines, concurrency,
model choice or tool permissions. Values remain untrusted context in the
existing bounded workflow input block. They cannot rewrite launch controls.
This is data routing, not human approval or authorization for additional tools.
Already completed branch effects are never rolled back because a continuation
cannot obtain its input. Error reports retain candidate node IDs via the
existing `blockedBy` field without echoing compared data.

## Verification

Ten added Rust regressions cover closed input shapes, direct-dependency and
join requirements, all two-candidate state combinations in both orders,
failure precedence, exact types/pointers, source provenance, selected arrays,
and the public schema. Four public `SubagentTool` workflows use actual native
child-process fixtures without contacting a model; they cover either branch,
ambiguous/failed alternatives, missing fields, intentional exclusions and
preflight of all candidate schemas. Existing workflow, decision and fan-out
regression files are unchanged.

The Rust tests are authored, not executed in this environment. The required
`dsr quality --tool pi_agent_rust` command is unavailable (exit 127). Rust
compilation, formatting, Clippy and runtime/cross-platform acceptance remain
unverified. Standalone shell and JSON fixture checks are not DSR acceptance.
