# Live control of an in-process SDK turn

`AgentSessionHandle::into_controllable()` adds a separate input/cancellation
lane to an existing SDK session. Unlike borrowing the session again, this lane
can be used while its prompt future is running. It feeds the agent's existing
steering/follow-up fetchers and retains the SDK's provider, tools, event
listeners, retry/failover handling, and session-recording path.

This is an in-process SDK API. It does not by itself enable busy-editor input
in the default FTUI frontend or change the existing subprocess RPC protocol.

## Prompt, live steering, and recovery

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use pi::sdk::{AgentEvent, AgentSessionHandle, AssistantMessage, Result};
use pi::session_control::PendingInput;

async fn inspect(
    handle: AgentSessionHandle,
) -> (Result<AssistantMessage>, Vec<PendingInput>) {
    let mut session = handle.into_controllable();
    let steered = AtomicBool::new(false);
    let turn = match session.prompt_with_control(
        "Inspect the repository and report the main implementation gaps".to_string(),
        move |control, event| {
            if matches!(event, AgentEvent::ToolExecutionStart { .. })
                && !steered.swap(true, Ordering::SeqCst)
            {
                // The SDK session is still borrowed by the running prompt.
                // This only takes the independent mailbox's short-lived lock.
                if let Err(error) = control.steer("Prioritize missing runtime functionality") {
                    eprintln!("Steering was not queued: {error}");
                }
            }
        },
    ) {
        Ok(turn) => turn,
        Err(error) => return (Err(error), Vec::new()),
    };
    let control = turn.control();
    // A GUI/terminal host may give control.clone() to its input thread here.
    let result = turn.await;
    // Reclaim even on an error, abort, or late-arriving input race.
    let unclaimed = control.take_pending();
    (result, unclaimed)
}
```

`prompt(input, on_event)` provides the ordinary SDK callback signature.
`prompt_with_control` also supplies the current handle to each callback,
without requiring a shared mutable session or an initialization handshake.
`continue_turn(on_event)` resumes through the existing SDK continuation path;
it does not append the original user prompt a second time.

## Input and cancellation semantics

`steer(text)` queues exact user-authored text for a steering boundary.
`follow_up(text)` queues input for a subsequent model turn. Each lane is FIFO;
the agent's existing safe boundaries determine when it consumes input. A
single fetch takes one item rather than pre-draining the entire backlog.

An `InputId` means **in-memory admission**, not durable storage, provider
receipt, or execution. `snapshot().handed_to_agent` counts handoffs only. Once
handed off, the normal agent history and persistence rules apply; callers must
not blindly replay that input after an uncertain failure.

`retract(id)` returns one still-unclaimed input. `take_pending()` returns all
unclaimed inputs in admission order. Both operations are serialized against
fetches, so an input cannot be both handed off and reclaimed. Receipts from an
old turn do not identify inputs in a new turn.

`abort()` stops further mailbox consumption and signals **only this turn**.
Keep awaiting the turn to let the agent perform its normal cancellation and
session-recording work. Dropping a turn closes its handle, even before the
first poll, but does not certify completion of asynchronous cleanup or disk
persistence. Old handles cannot enqueue into or abort a later turn.

Inputs left in the mailbox stay available through the caller's handle. They
are not silently copied into the next prompt. In particular, input can arrive
between the agent's last fetch and terminal events: always inspect/reclaim
pending input when a turn returns. Retain the control handle until recovery is
complete; dropping all owners is not durable storage.

## Attachments and authored-source tracking

`steer_with_content(&UserContent, authored_source)` and
`follow_up_with_content(&UserContent, authored_source)` preserve native text,
image and media blocks in order. `authored_source` is the user's original
prose **before** a host expands templates, file wrappers, or attachments. Only
that source is eligible for the agent's magic-keyword scan; an attached file
containing a keyword must not activate it accidentally. An image-only input
can supply an empty authored source.

Reclaimed `PendingInput` values contain both `text` (the original authored
source) and `content` (the provider-visible payload). Restore those together;
do not flatten an image into base64 prompt text. Queue admission does not open
paths or URLs, decode files, grant tool capabilities, or bypass provider input
validation. Thinking and tool-call blocks are not accepted as queued user
content.

## Bounds and lifecycle

The two queues share a limit of 100 pending inputs and 8 MiB of counted bytes.
Plain text and authored source are each limited to 256 KiB. Attachment-aware
content is limited to 128 blocks and 4 MiB when serialized; the aggregate
budget also counts its retained authored source. Overflow rejects the new
input without dropping the oldest input. Reclaiming or handing off an input
releases its mailbox budget. Debug output excludes input text and payloads.

The wrapper installs its two additive fetchers once. `session_mut()` permits
idle configuration and normal in-place session operations; it cannot be
borrowed while a controlled turn holds the session. Do not replace the entire
handle or its inner agent through that accessor, since doing so replaces the
fetchers too; construct a new controllable wrapper for a replacement handle.

## Validation scope

Regression coverage includes bounded admission, native blocks and authored
source, retraction/recovery, stale controls and delayed fetches, unpolled
future cleanup, and local HTTP/SSE scenarios using the production OpenAI
adapter, agent loop, read tool, and SDK wrapper. Run the repository's DSR
quality entry point to execute that coverage. Source review is not a passing
Rust build, test run, or release qualification.
