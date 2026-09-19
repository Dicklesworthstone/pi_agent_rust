//! Reconcile imported history into complete, non-executable tool exchanges.
//!
//! Rollouts may stop mid-tool, repeat ids, or put each parallel call in its own
//! assistant envelope. A native transcript must not contain dangling calls or
//! orphan results, and an import must never invent a successful tool output.
//! Retain only unambiguous completed pairs as protocol tools; everything else
//! remains explicitly historical text/details. Original converted sequences
//! are preserved in reconciliation audit entries whenever ordering changes.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use crate::model::{
    AssistantMessage, ContentBlock, CustomMessage, Message, StopReason, TextContent,
    ToolResultMessage,
};

pub(super) struct SourceRecord {
    pub line: usize,
    pub messages: Vec<Message>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Reconciliation {
    pub source_lines: Vec<usize>,
    pub reason: String,
    pub unresolved: bool,
    pub original_messages: Vec<Message>,
}

#[derive(Default)]
pub(super) struct Normalized {
    pub messages: Vec<Message>,
    pub reconciliations: Vec<Reconciliation>,
}

#[derive(Default)]
struct Counts {
    calls: usize,
    results: usize,
}

struct Batch {
    assistant: AssistantMessage,
    results: Vec<Arc<ToolResultMessage>>,
    lines: BTreeSet<usize>,
    original: Vec<Message>,
    assistant_count: usize,
}

impl Batch {
    fn new(line: usize, message: Arc<AssistantMessage>) -> Self {
        Self {
            assistant: message.as_ref().clone(),
            results: Vec::new(),
            lines: BTreeSet::from([line]),
            original: vec![Message::Assistant(message)],
            assistant_count: 1,
        }
    }

    fn can_join(&self, message: &AssistantMessage) -> bool {
        self.results.is_empty()
            && self.assistant.provider == message.provider
            && self.assistant.api == message.api
            && self.assistant.model == message.model
    }

    fn join(&mut self, line: usize, message: Arc<AssistantMessage>) {
        self.assistant
            .content
            .extend(message.content.iter().cloned());
        self.lines.insert(line);
        self.original.push(Message::Assistant(message));
        self.assistant_count += 1;
    }

    fn push_result(&mut self, line: usize, result: Arc<ToolResultMessage>) {
        self.lines.insert(line);
        self.original.push(Message::ToolResult(Arc::clone(&result)));
        self.results.push(result);
    }
}

fn has_calls(message: &AssistantMessage) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)))
}

pub(super) fn normalize(records: Vec<SourceRecord>) -> Normalized {
    let mut counts: HashMap<String, Counts> = HashMap::new();
    for record in &records {
        for message in &record.messages {
            match message {
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let ContentBlock::ToolCall(call) = block {
                            counts.entry(call.id.clone()).or_default().calls += 1;
                        }
                    }
                }
                Message::ToolResult(result) => {
                    counts
                        .entry(result.tool_call_id.clone())
                        .or_default()
                        .results += 1;
                }
                _ => {}
            }
        }
    }

    let mut out = Normalized::default();
    let mut pending: Option<Batch> = None;
    for record in records {
        let has_results = record
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)));
        let has_user = record
            .messages
            .iter()
            .any(|message| matches!(message, Message::User(_)));
        let only_user_envelope = record
            .messages
            .iter()
            .all(|message| matches!(message, Message::User(_) | Message::ToolResult(_)));
        let messages = if has_results && has_user && only_user_envelope {
            // Claude permits text and results in one user envelope. Native
            // messages split them, but all paired results must precede that
            // envelope's user text on a provider's next request. Never move a
            // result ahead of an assistant call in another record shape.
            out.reconciliations.push(Reconciliation {
                source_lines: vec![record.line],
                reason:
                    "placed co-record tool outputs before user text for a complete tool exchange"
                        .into(),
                unresolved: false,
                original_messages: record.messages.clone(),
            });
            let (mut results, others): (Vec<_>, Vec<_>) = record
                .messages
                .into_iter()
                .partition(|message| matches!(message, Message::ToolResult(_)));
            results.extend(others);
            results
        } else {
            record.messages
        };
        for message in messages {
            match message {
                Message::Assistant(assistant) if has_calls(&assistant) => {
                    if let Some(batch) = pending.as_mut().filter(|batch| batch.can_join(&assistant))
                    {
                        batch.join(record.line, assistant);
                    } else {
                        flush_batch(&mut pending, &counts, &mut out);
                        pending = Some(Batch::new(record.line, assistant));
                    }
                }
                Message::ToolResult(mut result) => {
                    if let Some(batch) = pending.as_mut() {
                        batch.push_result(record.line, result);
                    } else {
                        out.reconciliations.push(Reconciliation {
                            source_lines: vec![record.line],
                            reason: "tool output has no pending imported call; retained as historical context".into(),
                            unresolved: true,
                            original_messages: vec![Message::ToolResult(Arc::clone(&result))],
                        });
                        clear_ambiguous_name(&mut result, &counts);
                        out.messages.push(historical_result(&result));
                    }
                }
                other => {
                    // Never match a call across a new user turn or a later
                    // ordinary assistant response. A matching id alone is not
                    // proof that two events belong to the same tool exchange.
                    flush_batch(&mut pending, &counts, &mut out);
                    out.messages.push(other);
                }
            }
        }
    }
    flush_batch(&mut pending, &counts, &mut out);
    out
}

fn clear_ambiguous_name(result: &mut Arc<ToolResultMessage>, counts: &HashMap<String, Counts>) {
    if counts
        .get(&result.tool_call_id)
        .is_some_and(|count| count.calls > 1)
    {
        // The streaming reader may have filled a missing name from the first
        // occurrence before it discovered id reuse. Do not present that guess
        // as the name of an ambiguous result. The original remains in audit.
        Arc::make_mut(result).tool_name.clear();
    }
}

fn flush_batch(
    pending: &mut Option<Batch>,
    counts: &HashMap<String, Counts>,
    out: &mut Normalized,
) {
    let Some(mut batch) = pending.take() else {
        return;
    };
    // Indexed membership keeps large parallel batches out of a quadratic
    // call-by-output scan. Owned ids let the result vector be consumed below.
    let observed: BTreeSet<String> = batch
        .results
        .iter()
        .map(|result| result.tool_call_id.clone())
        .collect();
    let valid: HashMap<String, String> = batch
        .assistant
        .content
        .iter()
        .filter_map(|block| {
            let ContentBlock::ToolCall(call) = block else {
                return None;
            };
            let count = counts.get(&call.id)?;
            if count.calls == 1 && count.results == 1 && observed.contains(&call.id) {
                Some((call.id.clone(), call.name.clone()))
            } else {
                None
            }
        })
        .collect();
    let mut unresolved = false;
    for block in &mut batch.assistant.content {
        if let ContentBlock::ToolCall(call) = block
            && !valid.contains_key(&call.id)
        {
            // No fabricated result, and no pending executable ToolCall left at
            // EOF. Preserve the exact arguments in both text and audit data.
            let text = format!(
                "[Historical tool call {} ({}) has no unambiguous completed exchange in this import. Pi did not execute it.]\nArguments: {}",
                call.name, call.id, call.arguments
            );
            *block = ContentBlock::Text(TextContent::new(text));
            unresolved = true;
        }
    }
    batch.assistant.stop_reason = if valid.is_empty() {
        StopReason::Stop
    } else {
        StopReason::ToolUse
    };
    out.messages.push(Message::assistant(batch.assistant));

    // Complete protocol results first; unpaired outputs are historical context
    // only and may not interrupt the assistant/tool-result protocol group.
    let mut historical = Vec::new();
    for mut result in batch.results {
        if let Some(name) = valid.get(&result.tool_call_id) {
            Arc::make_mut(&mut result).tool_name.clone_from(name);
            out.messages.push(Message::ToolResult(result));
        } else {
            unresolved = true;
            clear_ambiguous_name(&mut result, counts);
            historical.push(historical_result(&result));
        }
    }
    out.messages.extend(historical);
    if unresolved || batch.assistant_count > 1 {
        out.reconciliations.push(Reconciliation {
            source_lines: batch.lines.into_iter().collect(),
            reason: if unresolved {
                "incomplete or ambiguous tool exchanges retained as non-executable history"
            } else {
                "combined adjacent foreign assistant tool-call envelopes into one native batch"
            }
            .to_string(),
            unresolved,
            original_messages: batch.original,
        });
    }
}

fn historical_result(result: &ToolResultMessage) -> Message {
    let mut text = format!(
        "[Historical tool output for {} ({}); no unambiguous paired call in this import. Recorded output, not a pending tool action. Error: {}]",
        if result.tool_name.is_empty() {
            "unknown tool"
        } else {
            &result.tool_name
        },
        result.tool_call_id,
        result.is_error,
    );
    for block in &result.content {
        text.push('\n');
        match block {
            ContentBlock::Text(content) => text.push_str(&content.text),
            _ => text.push_str("[Non-text output retained in import details]"),
        }
    }
    Message::Custom(CustomMessage {
        content: text,
        custom_type: "foreign_tool_result".to_string(),
        display: true,
        details: Some(json!({"toolResult": result})),
        timestamp: result.timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolCall, UserContent, UserMessage};

    fn calls(ids: &[&str]) -> Message {
        Message::assistant(AssistantMessage {
            content: ids
                .iter()
                .map(|id| {
                    ContentBlock::ToolCall(ToolCall {
                        id: (*id).to_string(),
                        name: format!("tool_{id}"),
                        arguments: json!({"id": id}),
                        thought_signature: None,
                    })
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            timestamp: 1,
            ..AssistantMessage::default()
        })
    }

    fn result(id: &str) -> Message {
        Message::tool_result(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: String::new(),
            content: vec![ContentBlock::Text(TextContent::new(format!("output {id}")))],
            is_error: id == "b",
            timestamp: 2,
            details: None,
        })
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 3,
        })
    }

    fn records(messages: Vec<Message>) -> Vec<SourceRecord> {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| SourceRecord {
                line: index + 1,
                messages: vec![message],
            })
            .collect()
    }

    /// Protocol postcondition: every tool batch has exactly its matching
    /// outputs before another conversational message, with no id reuse.
    fn assert_complete(messages: &[Message]) {
        let mut pending = HashMap::new();
        let mut seen = BTreeSet::new();
        for message in messages {
            match message {
                Message::ToolResult(result) => {
                    let name = pending.remove(&result.tool_call_id).expect("orphan result");
                    assert_eq!(result.tool_name, name);
                }
                other => {
                    assert!(pending.is_empty(), "interrupted tool batch: {pending:?}");
                    if let Message::Assistant(assistant) = other {
                        for block in &assistant.content {
                            if let ContentBlock::ToolCall(call) = block {
                                assert!(seen.insert(call.id.clone()), "duplicate native call id");
                                pending.insert(call.id.clone(), call.name.clone());
                            }
                        }
                        assert_eq!(
                            assistant.stop_reason == StopReason::ToolUse,
                            !pending.is_empty()
                        );
                    }
                }
            }
        }
        assert!(pending.is_empty(), "unfinished tool batch");
    }

    fn native_call_count(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(
                    assistant
                        .content
                        .iter()
                        .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                        .count(),
                ),
                _ => None,
            })
            .sum()
    }

    #[test]
    fn codex_parallel_call_envelopes_become_one_complete_native_batch() {
        let out = normalize(records(vec![
            calls(&["a"]),
            calls(&["b"]),
            result("b"),
            result("a"),
        ]));
        assert_complete(&out.messages);
        assert_eq!(out.messages.len(), 3);
        assert_eq!(native_call_count(&out.messages), 2);
        let Message::ToolResult(first) = &out.messages[1] else {
            panic!("tool result")
        };
        assert_eq!(first.tool_call_id, "b");
        assert_eq!(first.tool_name, "tool_b");
        assert!(first.is_error);
        assert!(out.reconciliations.iter().all(|notice| !notice.unresolved));
        assert_eq!(out.reconciliations[0].original_messages.len(), 4);
    }

    #[test]
    fn incomplete_final_call_is_history_not_an_executable_tool_request() {
        let out = normalize(records(vec![calls(&["delete_file"])]));
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 0);
        let Message::Assistant(last) = &out.messages[0] else {
            panic!("assistant")
        };
        assert_eq!(last.stop_reason, StopReason::Stop);
        let ContentBlock::Text(text) = &last.content[0] else {
            panic!("historical text")
        };
        assert!(text.text.contains("Pi did not execute it"));
        assert!(text.text.contains("delete_file"));
        assert!(out.reconciliations[0].unresolved);
        let Message::Assistant(original) = &out.reconciliations[0].original_messages[0] else {
            panic!("original")
        };
        assert!(matches!(&original.content[0], ContentBlock::ToolCall(_)));
    }

    #[test]
    fn orphan_output_keeps_exact_content_error_and_structured_details() {
        let out = normalize(records(vec![result("b")]));
        assert_complete(&out.messages);
        let Message::Custom(message) = &out.messages[0] else {
            panic!("historical output")
        };
        assert!(message.content.ends_with("output b"));
        assert!(message.content.contains("Error: true"));
        assert_eq!(
            message
                .details
                .as_ref()
                .and_then(|value| value.pointer("/toolResult/toolCallId"))
                .and_then(serde_json::Value::as_str),
            Some("b")
        );
        assert!(out.reconciliations[0].unresolved);
    }

    #[test]
    fn repeated_call_ids_are_not_guessed_or_replayed() {
        let out = normalize(records(vec![
            calls(&["a"]),
            result("a"),
            user("next"),
            calls(&["a"]),
            result("a"),
        ]));
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 0);
        assert_eq!(
            out.messages
                .iter()
                .filter(|message| matches!(message, Message::Custom(_)))
                .count(),
            2
        );
        assert!(out.reconciliations.iter().all(|notice| notice.unresolved));
    }

    #[test]
    fn duplicate_outputs_cannot_choose_an_arbitrary_result_as_truth() {
        let out = normalize(records(vec![calls(&["a"]), result("a"), result("a")]));
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 0);
        assert_eq!(out.messages.len(), 3);
        assert_eq!(
            out.messages
                .iter()
                .filter(|message| matches!(message, Message::Custom(_)))
                .count(),
            2
        );
    }

    #[test]
    fn call_ids_do_not_pair_across_a_new_user_turn() {
        let out = normalize(records(vec![
            calls(&["a"]),
            user("different turn"),
            result("a"),
        ]));
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 0);
        assert!(matches!(&out.messages[1], Message::User(_)));
        assert!(matches!(&out.messages[2], Message::Custom(_)));
    }

    #[test]
    fn mixed_claude_envelope_closes_tools_before_preserving_all_user_text() {
        let out = normalize(vec![
            SourceRecord {
                line: 1,
                messages: vec![calls(&["a", "b"])],
            },
            SourceRecord {
                line: 2,
                messages: vec![
                    user("before"),
                    result("b"),
                    user("between"),
                    result("a"),
                    user("after"),
                ],
            },
        ]);
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 2);
        assert_eq!(out.messages.len(), 6);
        let texts: Vec<&str> = out
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["before", "between", "after"]);
        assert_eq!(out.reconciliations[0].original_messages.len(), 5);
        assert!(!out.reconciliations[0].unresolved);
    }

    #[test]
    fn partial_batch_keeps_completed_pairs_without_inventing_other_outputs() {
        let out = normalize(records(vec![calls(&["a", "b"]), result("b")]));
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 1);
        assert_eq!(out.messages.len(), 2);
        let Message::ToolResult(result) = &out.messages[1] else {
            panic!("result")
        };
        assert_eq!(result.tool_call_id, "b");
        assert!(result.is_error);
        assert!(out.reconciliations[0].unresolved);
    }

    #[test]
    fn complete_output_permutations_preserve_pairing_and_arrival_order() {
        for ids in [
            ["a", "b", "c"],
            ["a", "c", "b"],
            ["b", "a", "c"],
            ["b", "c", "a"],
            ["c", "a", "b"],
            ["c", "b", "a"],
        ] {
            let mut messages = vec![calls(&["a"]), calls(&["b"]), calls(&["c"])];
            messages.extend(ids.iter().map(|id| result(id)));
            let out = normalize(records(messages));
            assert_complete(&out.messages);
            assert_eq!(native_call_count(&out.messages), 3);
            let actual: Vec<&str> = out
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(result.tool_call_id.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(actual, ids);
        }
    }

    #[test]
    fn missing_and_duplicated_output_matrix_always_produces_complete_native_history() {
        // 0, 1 or 2 observations for each of three calls: all 27 shapes.
        for a in 0..=2 {
            for b in 0..=2 {
                for c in 0..=2 {
                    let mut messages = vec![calls(&["a", "b", "c"])];
                    for (id, count) in [("a", a), ("b", b), ("c", c)] {
                        messages.extend((0..count).map(|_| result(id)));
                    }
                    let out = normalize(records(messages));
                    assert_complete(&out.messages);
                    assert_eq!(
                        native_call_count(&out.messages),
                        usize::from(a == 1) + usize::from(b == 1) + usize::from(c == 1)
                    );
                    assert_eq!(
                        out.messages
                            .iter()
                            .filter(|message| matches!(
                                message,
                                Message::ToolResult(_) | Message::Custom(_)
                            ))
                            .count(),
                        a + b + c
                    );
                }
            }
        }
    }

    #[test]
    fn a_native_call_and_result_in_one_record_are_not_reordered() {
        let out = normalize(vec![SourceRecord {
            line: 1,
            messages: vec![calls(&["a"]), result("a")],
        }]);
        assert_complete(&out.messages);
        assert_eq!(native_call_count(&out.messages), 1);
        assert_eq!(out.messages.len(), 2);
        assert!(out.reconciliations.is_empty());
    }

    #[test]
    fn ambiguous_outputs_do_not_inherit_a_guessed_name() {
        let Message::ToolResult(mut output) = result("a") else {
            unreachable!()
        };
        Arc::make_mut(&mut output).tool_name = "first_guess".to_string();
        let out = normalize(records(vec![
            calls(&["a"]),
            calls(&["a"]),
            Message::ToolResult(output),
        ]));
        assert_complete(&out.messages);
        let historical = out
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Custom(message) => Some(message),
                _ => None,
            })
            .expect("historical result");
        assert!(historical.content.contains("unknown tool"));
        assert!(!historical.content.contains("first_guess"));
    }
}
