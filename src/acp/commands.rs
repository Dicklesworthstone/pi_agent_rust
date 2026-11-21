//! ACP slash commands use the same live extension registrations and execution
//! boundary as the native hosts. Only authored wire text can invoke a command;
//! decoded resources and attachments are always model input.

use super::*;
use crate::model::{Message, StopReason, UserContent};
use crate::session::{SessionEntry, SessionMessage};
use std::collections::{BTreeMap, HashSet};

pub(super) enum PromptCommand {
    Mcp(mcp::Command),
    Extension { name: String, args: String },
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
    // /mcp remains an operator command even if an extension claims its name.
    if name.is_empty()
        || name == "mcp"
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
    if let Some(manager) = handle.and_then(AgentSessionHandle::extension_manager) {
        for command in manager.list_commands() {
            let Some(name) = command["name"].as_str() else {
                continue;
            };
            if name == "mcp" || !manager.has_callable_command(name) {
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

pub(super) async fn execute(
    handle: &mut AgentSessionHandle,
    name: &str,
    args: &str,
    abort: AbortSignal,
    out: &AcpOutput,
    id: &str,
    on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
) -> &'static str {
    let prior_entries = match existing_entries(handle).await {
        Ok(entries) => entries,
        Err(error) => return report_prompt_error(out, id, &error).await,
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
        return report_prompt_error(out, id, &error).await;
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
            Err(error) => return report_prompt_error(out, id, &error).await,
        }
    }

    let value = match result {
        Ok(value) => value,
        Err(error) => return report_prompt_error(out, id, &error).await,
    };
    if abort.is_aborted() {
        return ACP_STOP_REASON_CANCELLED;
    }
    let terminal = terminal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if matches!(terminal, Some((StopReason::Aborted, _, _))) {
        return ACP_STOP_REASON_CANCELLED;
    }
    if let Some((StopReason::Error, provider, message)) = terminal.as_ref() {
        return report_prompt_error(
            out,
            id,
            &Error::provider(
                provider.clone(),
                message.clone().unwrap_or_else(|| "Request failed".into()),
            ),
        )
        .await;
    }
    if !value.is_null() {
        let text = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_string);
        emit_text(out, id, &format!("/{name} → {text}")).await;
    }
    terminal.map_or(ACP_STOP_REASON_END_TURN, |(reason, _, _)| {
        map_stop_reason(reason)
    })
}
