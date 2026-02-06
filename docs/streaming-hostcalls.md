# Streaming Hostcall Results

Protocol extension for incremental delivery of hostcall results from Rust to
extension JavaScript.

## Status

**Draft** — bd-2tl1.1

## Motivation

The current hostcall protocol uses a simple request/response model:

```
Extension JS                         Rust Host
     │                                   │
     │── HostcallRequest { call_id } ──►│
     │                                   │  (process fully)
     │◄── HostcallOutcome::Success ─────│
     │                                   │
```

For long-running operations (exec with streaming stdout, large HTTP downloads,
file watches), the extension must wait for the entire result before receiving
any data. Streaming hostcalls allow incremental delivery:

```
Extension JS                         Rust Host
     │                                   │
     │── HostcallRequest { stream } ──►│
     │                                   │
     │◄── StreamChunk { seq=0 } ───────│  ← first partial
     │◄── StreamChunk { seq=1 } ───────│
     │◄── StreamChunk { seq=2, final } │  ← final chunk
     │                                   │
```

## Wire Format

### New `HostcallOutcome` Variant

```rust
pub enum HostcallOutcome {
    // Existing variants — unchanged.
    Success(serde_json::Value),
    Error { code: String, message: String },

    // NEW: incremental chunk delivery.
    StreamChunk {
        /// Monotonically increasing per (call_id). Starts at 0.
        sequence: u64,
        /// Arbitrary JSON payload (stdout line, HTTP body bytes, etc.).
        chunk: serde_json::Value,
        /// `true` on the last chunk. The stream is complete after this.
        is_final: bool,
    },
}
```

The `call_id` is not duplicated inside `StreamChunk` — it is already carried by
the enclosing `MacrotaskKind::HostcallComplete { call_id, outcome }`.

### New `MacrotaskKind` Variant

No new variant is needed. Each `StreamChunk` is delivered as an ordinary
`HostcallComplete` macrotask:

```rust
MacrotaskKind::HostcallComplete {
    call_id: "hc-42".into(),
    outcome: HostcallOutcome::StreamChunk {
        sequence: 0,
        chunk: json!("first line of stdout\n"),
        is_final: false,
    },
}
```

This reuses the existing scheduler queue and deterministic ordering without any
changes to the `Macrotask` struct or the `tick()` dispatch loop.

## Stream Lifecycle

### Happy Path

```
seq  call_id  outcome
───  ───────  ─────────────────────────────────────
 0   hc-42    StreamChunk { sequence: 0, chunk: "line 1\n", is_final: false }
 1   hc-42    StreamChunk { sequence: 1, chunk: "line 2\n", is_final: false }
 2   hc-42    StreamChunk { sequence: 2, chunk: "done\n",   is_final: true  }
```

After `is_final: true`, no further chunks are enqueued for `hc-42`. The
extension's async iterator yields `{ done: true }` on the next pull.

### Error Mid-Stream

If an error occurs after one or more chunks have been delivered, the host
enqueues a final `HostcallOutcome::Error` instead of another `StreamChunk`:

```
seq  call_id  outcome
───  ───────  ─────────────────────────────────────
 0   hc-42    StreamChunk { sequence: 0, chunk: "partial", is_final: false }
 1   hc-42    Error { code: "EXEC_FAILED", message: "exit code 1" }
```

The JS bridge converts this to an exception thrown from the iterator's
`next()` call. The stream is implicitly closed.

### Cancel Mid-Stream

The extension can cancel a stream by calling `pi.cancelStream(call_id)` (or
by dropping the async iterator). The host:

1. Stops producing chunks (kills the subprocess / aborts the HTTP request).
2. Enqueues a final `StreamChunk` with `is_final: true` and an empty chunk
   (`json!(null)`), so the JS side can clean up deterministically.
3. No further macrotasks are enqueued for this `call_id`.

If the host has already enqueued chunks that haven't been consumed yet, they
remain in the queue and are delivered normally. The final sentinel chunk is
always the last item enqueued.

### Zero-Chunk Stream

A streaming hostcall that produces no data before completing sends a single
chunk:

```
StreamChunk { sequence: 0, chunk: json!(null), is_final: true }
```

This is semantically equivalent to `Success(Value::Null)` but preserves the
streaming contract so the JS side always uses the same code path.

## Backpressure Model

### Problem

If Rust produces chunks faster than JS can consume them (e.g., a process
writing 10,000 lines/sec to stdout while the extension does async work per
line), unbounded buffering will exhaust memory.

### Mechanism: Bounded Channel

Each streaming hostcall creates a bounded channel between the Rust producer
and the scheduler enqueue point:

```
                     ┌─────────────────────┐
Rust producer ──────►│  bounded channel     │──────► Scheduler queue
  (exec/http)        │  capacity = 16       │        (macrotask FIFO)
                     └─────────────────────┘
```

**Capacity**: 16 chunks (configurable per-stream via `buffer_size` option,
default 16). This is the number of chunks that can be buffered between the
producer and the scheduler, *not* the total number of chunks in the macrotask
queue.

**Producer blocking**: When the channel is full, the Rust producer task
suspends (`channel.send().await`) until the consumer drains at least one slot.
This naturally rate-limits the producer to match JS consumption speed.

**Consumer pacing**: The JS side consumes chunks via `next()` calls on the
async iterator. Each `next()` call:

1. Resolves when the next `StreamChunk` macrotask is delivered via `tick()`.
2. After processing, the slot in the bounded channel is freed (the chunk has
   moved from channel → scheduler queue → JS delivery).

### Stall Detection

If the JS consumer does not call `next()` for **30 seconds** (the stall
timeout), the host treats the stream as abandoned:

1. The producer is cancelled (subprocess killed, HTTP aborted).
2. A final sentinel chunk (`is_final: true`, `chunk: null`) is enqueued.
3. A warning is logged: `"Stream stalled: JS consumer did not pull for 30s"`.

The stall timeout is measured from the moment the bounded channel becomes full
(i.e., the producer is blocked). If the channel never fills, no stall can
occur.

**Stall timeout** is configurable per-stream via the `stall_timeout_ms` option
(default: 30,000 ms). A value of 0 disables stall detection.

### Flow Diagram

```
                                              JS tick loop
                                              ┌──────────┐
Rust producer                                 │ tick()    │
┌──────────┐     bounded channel (cap=16)     │          │
│ exec     │──►  [c0][c1][c2]...[c15]  ──►   │ deliver_ │
│ stdout   │     ▲                            │ hostcall │
│          │     │ blocks when full            │ _complete│
└──────────┘     │                            │          │
                 │                            │ next()   │
                 └── slot freed when ─────────┘ pulls    │
                     chunk delivered               chunk  │
                     to JS                               │
                                              └──────────┘
```

## Scheduler Integration

### Ordering

Stream chunks use the existing `Seq`-based ordering. Each chunk gets its own
`Macrotask` with a unique `seq` assigned by `Scheduler::next_seq()` at enqueue
time. This guarantees:

1. **Per-stream ordering**: Chunks for the same `call_id` are enqueued in
   `sequence` order (0, 1, 2, ...) and therefore have ascending `seq` values.
   Since the macrotask queue is FIFO, they are delivered in order.

2. **Cross-stream interleaving**: When multiple streams are active, their
   chunks are interleaved in the global `seq` order. This is natural
   round-robin when producers yield at similar rates.

**Example** — two concurrent streams:

```
Global seq   call_id   sequence   is_final
─────────    ───────   ────────   ────────
    14       hc-42     0          false
    15       hc-99     0          false
    16       hc-42     1          false
    17       hc-99     1          true       ← hc-99 done
    18       hc-42     2          true       ← hc-42 done
```

Each `tick()` pops one macrotask (unchanged behavior). Stream chunks and
non-stream hostcall completions coexist in the same queue with no special
priority.

### No Reordering Guarantee

The scheduler does **not** reorder chunks. If chunks are enqueued out of
order (which should not happen with a single producer per stream), the
scheduler delivers them in enqueue order. The `sequence` field allows the JS
side to detect gaps if needed, but under normal operation no gaps occur.

### Determinism

Under `DeterministicClock`, stream chunk delivery is fully deterministic
because:
- Producers enqueue in a fixed order (determined by task scheduling).
- The FIFO queue preserves insertion order.
- `tick()` pops one at a time.

## Opt-In Mechanism

Streaming is opt-in per hostcall. The extension requests streaming by setting
`stream: true` in the hostcall payload:

```javascript
// Non-streaming (existing behavior, unchanged)
const result = await pi.exec("ls -la");
// result is the full output string

// Streaming (new)
const stream = await pi.exec("tail -f /var/log/syslog", { stream: true });
for await (const chunk of stream) {
  console.log("got:", chunk);
}
```

### Hostcall Kinds That Support Streaming

| Kind   | Streaming support | Chunk payload |
|--------|-------------------|---------------|
| `Exec` | Yes               | `string` (stdout/stderr line) |
| `Http` | Yes               | `string` (body chunk) |
| `Tool` | No                | — |
| `Session` | No             | — |
| `Events` | No              | — |
| `Ui`   | No                | — |

Non-streaming kinds ignore the `stream: true` flag and return a normal
`Success`/`Error` outcome.

### Detection in Rust Dispatch

```rust
fn dispatch_hostcall_allowed(
    &self,
    request: &HostcallRequest,
    // ...
) -> Result<()> {
    let wants_stream = request.payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match request.kind {
        HostcallKind::Exec if wants_stream => {
            self.dispatch_exec_streaming(request).await
        }
        HostcallKind::Exec => {
            self.dispatch_exec(request).await  // existing path
        }
        // ...
    }
}
```

## JS Bridge

### `deliver_hostcall_completion` Extension

The existing `deliver_hostcall_completion` function in `extensions_js.rs` is
extended to handle the new variant:

```rust
fn deliver_hostcall_completion(
    ctx: &Ctx<'_>,
    call_id: &str,
    outcome: &HostcallOutcome,
) -> rquickjs::Result<()> {
    let global = ctx.globals();
    let complete_fn: Function<'_> = global.get("__pi_complete_hostcall")?;

    let js_outcome = match outcome {
        HostcallOutcome::Success(value) => { /* existing */ }
        HostcallOutcome::Error { code, message } => { /* existing */ }
        HostcallOutcome::StreamChunk { sequence, chunk, is_final } => {
            let obj = Object::new(ctx.clone())?;
            obj.set("stream", true)?;
            obj.set("sequence", *sequence)?;
            obj.set("chunk", json_to_js(ctx, chunk)?)?;
            obj.set("isFinal", *is_final)?;
            obj
        }
    };

    complete_fn.call::<_, ()>((call_id, js_outcome))?;
    Ok(())
}
```

### JS-Side `__pi_complete_hostcall`

The JS-side handler checks for `outcome.stream`:

```javascript
function __pi_complete_hostcall(call_id, outcome) {
  const pending = __pi_pending_hostcalls.get(call_id);
  if (!pending) return;

  if (outcome.stream) {
    // Push chunk to the stream's internal buffer.
    // The async iterator's next() pulls from this buffer.
    pending.pushChunk(outcome.chunk, outcome.isFinal);
    if (outcome.isFinal) {
      __pi_pending_hostcalls.delete(call_id);
    }
    return;
  }

  // Non-streaming: existing resolve/reject logic.
  __pi_pending_hostcalls.delete(call_id);
  if (outcome.ok) {
    pending.resolve(outcome.value);
  } else {
    pending.reject(new Error(`${outcome.code}: ${outcome.message}`));
  }
}
```

### Async Iterator Implementation

Each streaming hostcall returns an object implementing the async iterator
protocol:

```javascript
class HostcallStream {
  constructor(callId) {
    this.callId = callId;
    this.buffer = [];       // chunks received but not yet pulled
    this.waitResolve = null; // resolve fn for pending next()
    this.done = false;
    this.error = null;
  }

  pushChunk(chunk, isFinal) {
    if (isFinal) {
      this.done = true;
    }
    if (this.waitResolve) {
      // Consumer is waiting — deliver immediately.
      const resolve = this.waitResolve;
      this.waitResolve = null;
      resolve({ value: chunk, done: isFinal && chunk === null });
    } else {
      // Buffer for later pull.
      this.buffer.push({ chunk, isFinal });
    }
  }

  async next() {
    if (this.buffer.length > 0) {
      const { chunk, isFinal } = this.buffer.shift();
      return { value: chunk, done: isFinal && chunk === null };
    }
    if (this.done) {
      return { value: undefined, done: true };
    }
    // Wait for next chunk delivery.
    return new Promise(resolve => {
      this.waitResolve = resolve;
    });
  }

  [Symbol.asyncIterator]() { return this; }
}
```

## Edge Cases

### 1. Cancel Mid-Stream

**Trigger**: Extension drops the async iterator (e.g., `break` in `for await`)
or calls `pi.cancelStream(callId)`.

**Sequence**:
1. JS calls `__pi_cancel_stream(call_id)` native function.
2. Rust receives cancel signal, kills subprocess / aborts HTTP.
3. Rust drains the bounded channel (discards buffered chunks).
4. Rust enqueues `StreamChunk { sequence: N, chunk: null, is_final: true }`.
5. JS iterator yields `{ done: true }` on next pull.

**Invariant**: Exactly one final chunk is always delivered, even on cancel.

### 2. Error Mid-Stream

**Trigger**: Subprocess exits non-zero, HTTP connection drops, timeout.

**Sequence**:
1. Producer detects error.
2. Producer enqueues `HostcallOutcome::Error { code, message }` for the
   `call_id` (not a `StreamChunk`).
3. JS bridge converts to exception thrown from `next()`.
4. No further chunks are enqueued.

**Note**: Chunks already buffered in the channel or macrotask queue are
delivered before the error. The error is always the last item for this
`call_id`.

### 3. Backpressure Stall

**Trigger**: JS consumer stops calling `next()` while producer has data.

**Sequence**:
1. Producer fills bounded channel (16 chunks).
2. Producer blocks on `channel.send().await`.
3. Stall timer starts (30s default).
4. After 30s with no consumer progress:
   - Producer is cancelled.
   - Final sentinel chunk enqueued.
   - Warning logged.

**Recovery**: The extension can catch the stall by handling the final chunk
and inspecting the sentinel value (`null`).

### 4. Extension Unload During Stream

**Trigger**: Extension is unloaded (e.g., `ExtensionRegion` dropped) while a
stream is active.

**Sequence**:
1. `ExtensionRegion::drop()` initiates cleanup with budget.
2. All active streams for this extension are cancelled (same as cancel
   mid-stream).
3. Bounded channels are dropped, which unblocks producers.
4. Producers detect the closed channel and stop.

### 5. Multiple Concurrent Streams

Multiple streams from the same extension or different extensions coexist
without interference:

- Each stream has its own bounded channel.
- Each stream has its own `sequence` counter (starts at 0).
- The scheduler interleaves chunks from all streams in global `seq` order.
- Backpressure is per-stream (one slow consumer does not block others).

### 6. Stream With `DeterministicClock`

Under deterministic testing:
- Producers enqueue all chunks synchronously (no real I/O).
- The macrotask queue contains all chunks in a known order.
- `tick()` delivers one at a time, allowing assertions after each chunk.

## Configuration

| Parameter | Default | Scope | Description |
|-----------|---------|-------|-------------|
| `stream` | `false` | per-call | Enable streaming for this hostcall |
| `buffer_size` | `16` | per-call | Bounded channel capacity (chunks) |
| `stall_timeout_ms` | `30000` | per-call | Max idle time before auto-cancel (0 = disabled) |

These are passed in the hostcall payload:

```javascript
const stream = await pi.exec("make build", {
  stream: true,
  buffer_size: 32,       // larger buffer for bursty output
  stall_timeout_ms: 0,   // disable stall detection
});
```

## Backward Compatibility

- Non-streaming hostcalls are completely unchanged.
- The `stream: true` flag is ignored by hostcall kinds that don't support it.
- Extensions that don't use streaming see no behavioral difference.
- The `HostcallOutcome::StreamChunk` variant is additive — existing match arms
  on `Success`/`Error` continue to work (Rust will require a new arm, but
  that's a compile-time check, not a runtime break).
