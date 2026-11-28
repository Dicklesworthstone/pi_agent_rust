//! ACP slash commands use native context maintenance and the same live extension
//! registrations as the native hosts. Only authored wire text can invoke a command;
//! decoded resources and attachments are always model input.

use super::*;
use crate::compaction::ManualCompactionMode;
use crate::model::{Message, StopReason, UserContent};
use crate::session::{SessionEntry, SessionMessage};
use std::collections::{BTreeMap, HashSet};

pub(super) enum PromptCommand {
    Context(ContextCommand),
    Mcp(mcp::Command),
    Extension { name: String, args: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ContextCommand {
    Compact {
        mode: ManualCompactionMode,
        instructions: Option<String>,
    },
    Handoff {
        focus: String,
    },
}

fn reserved(name: &str) -> bool {
    ["compact", "shake", "handoff", "mcp"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

pub(super) fn context_from_prompt(
    blocks: &[Value],
) -> std::result::Result<Option<PromptCommand>, String> {
    let Some(text) = blocks
        .first()
        .filter(|block| block["type"] == "text")
        .and_then(|block| block["text"].as_str())
    else {
        return Ok(None);
    };
    let Some(command) = text.trim_start().strip_prefix('/') else {
        return Ok(None);
    };
    let (name, args) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    if !["compact", "shake", "handoff"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        return Ok(None);
    }
    if blocks.len() != 1 {
        return Err(
            "A context command must be one standalone text block without attachments".into(),
        );
    }
    let args = args.trim();
    let context = if name.eq_ignore_ascii_case("handoff") {
        let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        if first.starts_with('-') && first != "--" {
            return Err(
                "ACP supports /handoff [focus] in the current session. Report, preview, export and import options are not available here. Use /handoff -- <focus> for focus beginning with '-'.".into(),
            );
        }
        let focus = if first == "--" { rest.trim() } else { args };
        if focus.len() > 4096 || focus.contains('\0') {
            return Err(
                "Handoff focus must contain at most 4096 UTF-8 bytes and no NUL characters".into(),
            );
        }
        ContextCommand::Handoff {
            focus: focus.into(),
        }
    } else {
        let (mode, instructions) = if name.eq_ignore_ascii_case("shake") {
            (ManualCompactionMode::Shake, args)
        } else {
            let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
            if first.eq_ignore_ascii_case("shake") || first.eq_ignore_ascii_case("--shake") {
                (ManualCompactionMode::Shake, rest.trim())
            } else if first.eq_ignore_ascii_case("aggressive")
                || first.eq_ignore_ascii_case("--aggressive")
            {
                (ManualCompactionMode::Aggressive, rest.trim())
            } else {
                (ManualCompactionMode::Summary, args)
            }
        };
        ContextCommand::Compact {
            mode,
            instructions: (!instructions.is_empty()).then(|| instructions.to_string()),
        }
    };
    Ok(Some(PromptCommand::Context(context)))
}

pub(super) fn extension_from_prompt(
    blocks: &[Value],
    handle: Option<&AgentSessionHandle>,
) -> std::result::Result<Option<PromptCommand>, String> {
    let Some(text) = blocks
        .first()
        .filter(|block| block["type"] == "text")
        .and_then(|block| block["text"].as_str())
    else {
        return Ok(None);
    };
    let Some(command) = text.trim_start().strip_prefix('/') else {
        return Ok(None);
    };
    let (name, args) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    // Native operator commands cannot be replaced by an extension registration.
    if name.is_empty()
        || reserved(name)
        || !handle
            .and_then(AgentSessionHandle::extension_manager)
            .is_some_and(|manager| manager.has_command(name))
    {
        return Ok(None);
    }
    if blocks.len() != 1 {
        return Err(
            "An extension command must be one standalone text block without attachments"
                .to_string(),
        );
    }
    Ok(Some(PromptCommand::Extension {
        name: name.to_string(),
        args: args.trim_start().to_string(),
    }))
}

pub(super) fn available(handle: Option<&AgentSessionHandle>, has_mcp: bool) -> Vec<Value> {
    let mut commands = BTreeMap::new();
    if handle.is_some() {
        for (name, description, hint) in [
            (
                "compact",
                "Compact older conversation context while preserving recent turns",
                "[shake|aggressive] [focus]",
            ),
            (
                "shake",
                "Drop bulky older tool output without a model request",
                "[focus]",
            ),
            (
                "handoff",
                "Save a continuation brief in this conversation using the current model",
                "[focus]",
            ),
        ] {
            commands.insert(
                name.to_string(),
                json!({
                    "name": name, "description": description, "input": { "hint": hint },
                }),
            );
        }
    }
    if let Some(manager) = handle.and_then(AgentSessionHandle::extension_manager) {
        for command in manager.list_commands() {
            let Some(name) = command["name"].as_str() else {
                continue;
            };
            if reserved(name) || !manager.has_callable_command(name) {
                continue;
            }
            commands.entry(name.to_string()).or_insert_with(|| {
                json!({
                    "name": name,
                    "description": command["description"].as_str().unwrap_or("Extension command"),
                    "input": { "hint": "arguments" },
                })
            });
        }
    }
    if has_mcp {
        commands.insert("mcp".to_string(), mcp::command_metadata());
    }
    commands.into_values().collect()
}

pub(super) fn notification(id: &str, commands: &[Value]) -> String {
    json_rpc_notification(
        "session/update",
        json!({
            "sessionId": id,
            "update": {
                "sessionUpdate": "available_commands_update",
                "availableCommands": commands,
            },
        }),
    )
}

pub(super) fn for_state(id: &str, state: &AcpSessionState) -> Option<String> {
    let commands = available(state.agent_session.as_ref(), state.mcp.is_some());
    (!commands.is_empty()).then(|| notification(id, &commands))
}

/// Non-triggering sendMessage calls append directly without AgentEvents.
/// Remember entry identities so completion can publish only newly committed,
/// displayable messages, including those generated without a provider turn.
async fn existing_entries(handle: &AgentSessionHandle) -> Result<HashSet<String>> {
    handle
        .with_session(|session| {
            session
                .entries
                .iter()
                .filter_map(SessionEntry::base_id)
                .cloned()
                .collect()
        })
        .await
}

async fn emit_text(out: &AcpOutput, id: &str, text: &str) {
    let _ = history::send_line(
        out,
        json_rpc_notification(
            "session/update",
            json!({
                "sessionId": id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text },
                },
            }),
        ),
    )
    .await;
}

/// Run the native durable transaction, not an ordinary user/model turn. The
/// existing prompt reservation owns cancellation and queued input throughout;
/// keep awaiting after cancellation so an admitted save can settle or fence.
pub(super) async fn execute_context(
    handle: &mut AgentSessionHandle,
    command: ContextCommand,
    abort: AbortSignal,
    out: &AcpOutput,
    id: &str,
    on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
) -> &'static str {
    let (progress, complete) = match &command {
        ContextCommand::Compact {
            mode: ManualCompactionMode::Shake,
            ..
        } => (
            "Dropping older tool output…\n",
            "Older tool output dropped; continuing with compacted context.",
        ),
        ContextCommand::Compact {
            mode: ManualCompactionMode::Aggressive,
            ..
        } => (
            "Compacting conversation aggressively…\n",
            "Conversation compacted aggressively; recent complete turns are preserved.",
        ),
        ContextCommand::Compact { .. } => (
            "Compacting conversation…\n",
            "Conversation compacted; recent complete turns are preserved.",
        ),
        ContextCommand::Handoff { .. } => (
            "Preparing a continuation brief…\n",
            "Handoff saved; continuing in this session with compacted context.",
        ),
    };
    // Do not publish progress for a request whose owner was already cancelled.
    if abort.is_aborted() {
        return ACP_STOP_REASON_CANCELLED;
    }
    emit_text(out, id, progress).await;
    let result = match command {
        ContextCommand::Compact { mode, instructions } => {
            handle
                .compact_in_mode_with_abort(mode, instructions.as_deref(), abort, on_event)
                .await
        }
        ContextCommand::Handoff { focus } => {
            handle.handoff(&focus, &abort, on_event).await.map(|()| true)
        }
    };
    match result {
        Ok(changed) => {
            // Native success is the durable outcome. A cancellation arriving
            // after installation must not turn it into a retryable failure.
            let message = if changed {
                complete
            } else {
                "Compaction skipped: there is no older context outside the retained recent turns."
            };
            emit_text(out, id, message).await;
            ACP_STOP_REASON_END_TURN
        }
        Err(error) => report_prompt_error(out, id, &error).await,
    }
}

pub(super) struct CommandOutcome {
    pub(super) stop_reason: &'static str,
    pub(super) succeeded: bool,
}

impl CommandOutcome {
    fn failed(stop_reason: &'static str) -> Self {
        Self { stop_reason, succeeded: false }
    }
}

pub(super) async fn execute(
    handle: &mut AgentSessionHandle,
    name: &str,
    args: &str,
    abort: AbortSignal,
    out: &AcpOutput,
    id: &str,
    on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
) -> CommandOutcome {
    let prior_entries = match existing_entries(handle).await {
        Ok(entries) => entries,
        Err(error) => return CommandOutcome::failed(report_prompt_error(out, id, &error).await),
    };
    let terminal = Arc::new(StdMutex::new(None));
    let observed_terminal = Arc::clone(&terminal);
    let event_out = out.clone();
    let event_id = id.to_string();
    let result = handle
        .execute_extension_command_with_abort(
            name,
            args,
            crate::extensions::EXTENSION_COMMAND_BUDGET_MS,
            Some(abort.clone()),
            move |event| {
                match &event {
                    AgentEvent::MessageStart {
                        message: Message::User(user),
                    } => {
                        if let UserContent::Text(text) = &user.content {
                            let _ = event_out.send(json_rpc_notification(
                                "session/update",
                                json!({
                                    "sessionId": event_id,
                                    "update": {
                                        "sessionUpdate": "user_message_chunk",
                                        "content": { "type": "text", "text": text },
                                    },
                                }),
                            ));
                        }
                    }
                    AgentEvent::AgentEnd { messages, .. } => {
                        if let Some(message) = messages.iter().rev().find_map(|message| {
                            if let Message::Assistant(message) = message {
                                Some(message)
                            } else {
                                None
                            }
                        }) {
                            let mut terminal = observed_terminal
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            // A command may queue several native runs without the
                            // SDK retry wrapper. Later success cannot erase an
                            // earlier failed run. Observe AgentEnd so recovered
                            // attempts and final tool-budget errors are settled.
                            if !terminal.as_ref().is_some_and(|(reason, _, _)| {
                                matches!(reason, StopReason::Error | StopReason::Aborted)
                            }) {
                                *terminal = Some((
                                    message.stop_reason,
                                    message.provider.clone(),
                                    message.error_message.clone(),
                                ));
                            }
                        }
                    }
                    _ => {}
                }
                on_event(event);
            },
        )
        .await;

    // sendMessage without triggerTurn, appendEntry and selection changes may
    // not run a provider turn. Settle their durable state before acknowledging
    // the command, including mutations made before an error or cancellation.
    // This boundary preserves the shared persistence quarantine and cost journal.
    if !result.as_ref().is_err_and(Error::is_session_persistence)
        && let Err(error) = handle.session_mut().persist_session().await
    {
        return CommandOutcome::failed(report_prompt_error(out, id, &error).await);
    }

    // Persistence failure must not make an uncommitted message look durable.
    if !result.as_ref().is_err_and(Error::is_session_persistence) {
        let messages = handle
            .with_session(|session| {
                session
                    .entries_for_current_path()
                    .into_iter()
                    .filter(|entry| {
                        entry
                            .base_id()
                            .is_some_and(|id| !prior_entries.contains(id))
                    })
                    .filter_map(|entry| match entry {
                        SessionEntry::Message(entry) => match &entry.message {
                            SessionMessage::Custom {
                                content,
                                display: true,
                                ..
                            } => Some(content.clone()),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .await;
        match messages {
            Ok(messages) => {
                for message in messages {
                    emit_text(out, id, &message).await;
                }
            }
            Err(error) => return CommandOutcome::failed(report_prompt_error(out, id, &error).await),
        }
    }

    let value = match result {
        Ok(value) => value,
        Err(error) => return CommandOutcome::failed(report_prompt_error(out, id, &error).await),
    };
    if abort.is_aborted() {
        return CommandOutcome::failed(ACP_STOP_REASON_CANCELLED);
    }
    let terminal = terminal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if matches!(terminal, Some((StopReason::Aborted, _, _))) {
        return CommandOutcome::failed(ACP_STOP_REASON_CANCELLED);
    }
    if let Some((StopReason::Error, provider, message)) = terminal.as_ref() {
        return CommandOutcome::failed(report_prompt_error(
            out,
            id,
            &Error::provider(
                provider.clone(),
                message.clone().unwrap_or_else(|| "Request failed".into()),
            ),
        ).await);
    }
    if !value.is_null() {
        let text = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_string);
        emit_text(out, id, &format!("/{name} → {text}")).await;
    }
    CommandOutcome {
        stop_reason: terminal.map_or(ACP_STOP_REASON_END_TURN, |(reason, _, _)| map_stop_reason(reason)),
        succeeded: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ContextCommand {
        let Some(PromptCommand::Context(command)) =
            context_from_prompt(&[json!({"type": "text", "text": text})]).unwrap()
        else {
            panic!("expected native context command");
        };
        command
    }

    #[test]
    fn context_command_modes_preserve_literal_focus() {
        for (text, mode, focus) in [
            (" /COMPACT ", ManualCompactionMode::Summary, None),
            ("/compact Keep  IDs\nnext line", ManualCompactionMode::Summary, Some("Keep  IDs\nnext line")),
            ("/compact --aggressive Keep  IDs", ManualCompactionMode::Aggressive, Some("Keep  IDs")),
            ("/compact AGGRESSIVE\tKeep  IDs", ManualCompactionMode::Aggressive, Some("Keep  IDs")),
            ("/compact --shake Keep  IDs", ManualCompactionMode::Shake, Some("Keep  IDs")),
            ("/compact shake", ManualCompactionMode::Shake, None),
            ("/SHAKE Keep  IDs", ManualCompactionMode::Shake, Some("Keep  IDs")),
            ("/compact aggressively preserve this", ManualCompactionMode::Summary, Some("aggressively preserve this")),
        ] {
            assert_eq!(parse(text), ContextCommand::Compact {
                mode, instructions: focus.map(str::to_string),
            });
        }
        assert_eq!(parse("/handoff -- -keep  IDs"), ContextCommand::Handoff { focus: "-keep  IDs".into() });
        assert_eq!(parse("/handoff"), ContextCommand::Handoff { focus: String::new() });
        for args in ["--report", "--model", "--export somewhere", "--import somewhere", "--unknown"] {
            assert!(context_from_prompt(&[json!({"type": "text", "text": format!("/handoff {args}")})]).is_err());
        }
        for focus in ["x".repeat(4097), "private\0focus".into()] {
            assert!(context_from_prompt(&[json!({"type": "text", "text": format!("/handoff {focus}")})]).is_err());
        }
    }

    #[test]
    fn resources_attachments_and_similar_names_cannot_gain_command_authority() {
        for blocks in [
            vec![json!({"type": "resource", "resource": {"uri": "file:///context", "text": "/shake"}})],
            vec![json!({"type": "resource_link", "uri": "/compact", "name": "context"})],
            vec![json!({"type": "text", "text": "Explain /compact"})],
            vec![json!({"type": "text", "text": "/compactly"})],
        ] {
            assert!(context_from_prompt(&blocks).unwrap().is_none());
        }
        for command in ["/compact", "/shake", "/handoff"] {
            for attachment in [
                json!({"type": "image", "mimeType": "image/png", "data": "YQ=="}),
                json!({"type": "text", "text": "second authored block"}),
                json!({"type": "resource", "resource": {"uri": "file:///context", "text": "notes"}}),
            ] {
                assert!(context_from_prompt(&[json!({"type": "text", "text": command}), attachment]).is_err());
            }
            assert!(reserved(command.trim_start_matches('/')));
        }
        assert!(reserved("HANDOFF"));
        assert!(!reserved("handoff-custom"));
    }
}
