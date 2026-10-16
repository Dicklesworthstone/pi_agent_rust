# Batched data-driven subagent work

`workflow` nodes can add an explicit `batchSize` to `forEach` to process larger
discovered worklists without starting one child per source value. Every batch
still uses the ordinary native child runner, with the same shared concurrency,
workspace baseline, schema correction, model choice and original deadline.
This is bounded fresh-child batching, not a retained OMP workpool or a new
scheduler. There is no speedup claim without comparative execution evidence.

```json
{
  "workflow": [
    {
      "id": "discover",
      "call": {
        "agent": "scout",
        "task": "Return the file paths relevant to the review, as a JSON array.",
        "outputSchema": {"type": "array", "items": {"type": "string"}}
      }
    },
    {
      "id": "reviews",
      "needs": ["discover"],
      "forEach": {
        "from": "discover", "pointer": "", "as": "batch",
        "maxItems": 8, "batchSize": 64
      },
      "call": {
        "agent": "reviewer",
        "task": "Review every path in batch.items. Return a summary with the original offset and findings for this batch.",
        "outputSchema": {
          "type": "object", "required": ["offset", "findings"],
          "properties": {
            "offset": {"type": "integer"},
            "findings": {"type": "array", "items": {"type": "string"}}
          }
        }
      }
    },
    {
      "id": "summary", "needs": ["reviews"],
      "inputs": {"reports": {"from": "reviews", "pointer": ""}},
      "call": {
        "agent": "reviewer",
        "task": "Reconcile the ordered batch reports into one review. Report missing evidence explicitly."
      }
    }
  ],
  "concurrency": 4,
  "timeoutSeconds": 300
}
```

Here at most 512 discovered paths become at most eight native reviewer calls.
Each reviewer receives the normal workflow input block with a `batch` value:

```json
{"offset":128,"total":130,"items":["src/one.rs","src/two.rs"]}
```

This is the final two-value batch of a 130-value source. Every nonfinal
batch has exactly `batchSize` entries. `offset` is the zero-based position in
the original array, `total` is that full array's length, and `items` contains
consecutive unmodified values. The final batch may be shorter. Order, nulls,
booleans, nested objects, Unicode and literal strings are preserved. Values
remain untrusted model context, never selectors for agent/model/tools/cwd,
isolation, permissions or deadlines. Shared `inputs`, including selected-branch
inputs, are resolved by the existing common input writer.

## Explicit shape and limits

Omitting `batchSize` keeps the existing scalar-item behavior byte-for-byte.
An explicit `batchSize: 1` uses the envelope too; it is not the same input shape
as omission. Nulls, strings, floats, zero, and values over 128 are rejected
before execution. Unknown options remain errors.

`maxItems` is still the explicit 1–32 **native task-instance ceiling** for this
node. In batched mode, the raw source-array limit is `maxItems * batchSize`,
up to 4,096 values. Exceeding that capacity fails the entire expansion rather
than truncating or processing a prefix. Across the graph, declared maxima plus
ordinary calls must still fit 128 native task instances; batching does not
multiply concurrency or the number of admitted processes. Native schema retries
retain their existing separately bounded behavior.

Every batch is counted before making its JSON copy, then its complete expanded
prompt—including authored task, shared inputs and escaped framing—must fit the
existing 64 KiB task limit. All batch prompts are prepared before any worker for
that node is submitted. A bad final batch therefore cannot leave an earlier
prefix running. Large individual values or shared context can make a batch too
large despite fitting its item count; choose a smaller explicit batch size or
return smaller source values. No automatic splitting or extra requests occur.

The original workflow deadline covers discovery, preparation, queueing,
execution, corrective retries and reduction. Conditional exclusion happens
before expansion. Empty arrays complete without workers and supply an empty
aggregate through the existing result path.

## Result and failure granularity

`fanout.itemCount` and each `fanout.items[index]` describe **native batches**, not
individual source values. Each batch has its actual child result, hub ID and
acceptance state. The node's `outputSchema` validates one whole worker result.
When all workers succeed with validated data, `fanout.data` is their ordered
array under the existing 256 KiB aggregate limit. It is not flattened, and the
host does not guess per-source-item yields or verify that the model addressed
every input. Include explicit coverage fields in the worker output schema and
check them in the reducer when needed.

A rejected or schema-invalid worker rejects the fan-out node and blocks its
dependants. Other completed worker results remain inspectable, but a partial
aggregate is not presented as complete. Already accepted edits and external
effects are not rolled back. There is no batch replay, worker pooling, automatic
resume, or new acceptance path.

## Validation

`src/subagents/workflow/fanout/batch_tests.rs` covers exact partitions, scalar
compatibility, provenance, typed values, expansion/prompt bounds and launch
policy. Four public native-child fixtures cover 97-value reduction, a rejected
batch, refusal before prefix launch, and empty worklists. The native fixtures
require Python 3 on Unix; they speak the real child protocol without calling a
model. Rust tests are authored, not executed in the implementation environment.
Acceptance remains `dsr quality --tool pi_agent_rust`; DSR was unavailable.
