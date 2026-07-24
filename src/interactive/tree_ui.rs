use super::*;

impl PiApp {
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_tree_ui_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let tree_ui = self.tree_ui.take()?;

        match tree_ui {
            TreeUiState::Selector(mut selector) => {
                match key.key_type {
                    KeyType::Up => selector.move_selection(-1),
                    KeyType::Down => selector.move_selection(1),
                    KeyType::CtrlU => {
                        selector.user_only = !selector.user_only;
                        if let Ok(session_guard) = self.session.try_lock() {
                            selector.rebuild(&session_guard);
                        }
                    }
                    KeyType::CtrlO => {
                        selector.show_all = !selector.show_all;
                        if let Ok(session_guard) = self.session.try_lock() {
                            selector.rebuild(&session_guard);
                        }
                    }
                    KeyType::Esc | KeyType::CtrlC => {
                        self.status_message = Some("Tree navigation cancelled".to_string());
                        self.tree_ui = None;
                        return None;
                    }
                    KeyType::Enter => {
                        if selector.rows.is_empty() {
                            self.tree_ui = None;
                            return None;
                        }

                        let selected = selector.rows[selector.selected].clone();
                        selector.last_selected_id = Some(selected.id.clone());

                        let (new_leaf_id, editor_text) = if let Some(text) = selected.resubmit_text
                        {
                            (selected.parent_id.clone(), Some(text))
                        } else {
                            (Some(selected.id.clone()), None)
                        };

                        // No-op if already at target leaf.
                        if selector.current_leaf_id.as_deref() == new_leaf_id.as_deref() {
                            self.status_message = Some("Already on that branch".to_string());
                            self.tree_ui = None;
                            return None;
                        }

                        let Ok(session_guard) = self.session.try_lock() else {
                            self.status_message = Some("Session busy; try again".to_string());
                            self.tree_ui = None;
                            return None;
                        };

                        let old_leaf_id = session_guard.leaf_id.clone();
                        let (entries_to_summarize, summary_from_id) = collect_tree_branch_entries(
                            &session_guard,
                            old_leaf_id.as_deref(),
                            new_leaf_id.as_deref(),
                        );
                        let session_id = session_guard.header.id.clone();
                        drop(session_guard);

                        let api_key_present = self.agent.try_lock().is_ok_and(|agent_guard| {
                            agent_guard.stream_options().api_key.is_some()
                        });

                        let pending = PendingTreeNavigation {
                            session_id,
                            old_leaf_id,
                            new_leaf_id,
                            editor_text,
                            entries_to_summarize,
                            summary_from_id,
                            api_key_present,
                        };

                        if pending.entries_to_summarize.is_empty() {
                            // Nothing to summarize; switch immediately.
                            if !self.start_tree_navigation(
                                pending,
                                TreeSummaryChoice::NoSummary,
                                None,
                            ) {
                                self.tree_ui = Some(TreeUiState::Selector(selector));
                            }
                            return None;
                        }

                        self.tree_ui = Some(TreeUiState::SummaryPrompt(TreeSummaryPromptState {
                            pending,
                            selected: 0,
                        }));
                        return None;
                    }
                    _ => {}
                }

                self.tree_ui = Some(TreeUiState::Selector(selector));
            }
            TreeUiState::SummaryPrompt(mut prompt) => {
                match key.key_type {
                    KeyType::Up if prompt.selected > 0 => {
                        prompt.selected -= 1;
                    }
                    KeyType::Down
                        if prompt.selected < TreeSummaryChoice::all().len().saturating_sub(1) =>
                    {
                        prompt.selected += 1;
                    }
                    KeyType::Esc | KeyType::CtrlC => {
                        self.status_message = Some("Tree navigation cancelled".to_string());
                        self.tree_ui = None;
                        return None;
                    }
                    KeyType::Enter => {
                        let choice = TreeSummaryChoice::all()[prompt.selected];
                        match choice {
                            TreeSummaryChoice::NoSummary | TreeSummaryChoice::Summarize => {
                                let pending = prompt.pending.clone();
                                if !self.start_tree_navigation(pending, choice, None) {
                                    self.tree_ui = Some(TreeUiState::SummaryPrompt(prompt));
                                }
                                return None;
                            }
                            TreeSummaryChoice::SummarizeWithCustomPrompt => {
                                self.tree_ui =
                                    Some(TreeUiState::CustomPrompt(TreeCustomPromptState {
                                        pending: prompt.pending,
                                        instructions: String::new(),
                                    }));
                                return None;
                            }
                        }
                    }
                    _ => {}
                }
                self.tree_ui = Some(TreeUiState::SummaryPrompt(prompt));
            }
            TreeUiState::CustomPrompt(mut custom) => {
                match key.key_type {
                    KeyType::Esc | KeyType::CtrlC => {
                        self.tree_ui = Some(TreeUiState::SummaryPrompt(TreeSummaryPromptState {
                            pending: custom.pending,
                            selected: 2,
                        }));
                        return None;
                    }
                    KeyType::Backspace => {
                        custom.instructions.pop();
                    }
                    KeyType::Enter => {
                        let pending = custom.pending.clone();
                        let instructions = if custom.instructions.trim().is_empty() {
                            None
                        } else {
                            Some(custom.instructions.clone())
                        };
                        if !self.start_tree_navigation(
                            pending,
                            TreeSummaryChoice::SummarizeWithCustomPrompt,
                            instructions,
                        ) {
                            self.tree_ui = Some(TreeUiState::CustomPrompt(custom));
                        }
                        return None;
                    }
                    KeyType::Runes => {
                        for ch in key.runes.iter().copied() {
                            custom.instructions.push(ch);
                        }
                    }
                    _ => {}
                }
                self.tree_ui = Some(TreeUiState::CustomPrompt(custom));
            }
        }

        None
    }

    /// Handle keyboard input when the branch picker overlay is active.
    pub fn handle_branch_picker_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let picker = self.branch_picker.as_mut()?;

        match key.key_type {
            KeyType::Up => picker.select_prev(),
            KeyType::Down => picker.select_next(),
            KeyType::PgUp => picker.select_page_up(),
            KeyType::PgDown => picker.select_page_down(),
            KeyType::Runes if key.runes == ['k'] => picker.select_prev(),
            KeyType::Runes if key.runes == ['j'] => picker.select_next(),
            KeyType::Enter => {
                if let Some(branch) = picker.selected_branch().cloned() {
                    if self.switch_to_branch_leaf(&branch.leaf_id) {
                        self.branch_picker = None;
                    }
                    return None;
                }
                self.branch_picker = None;
            }
            KeyType::Esc | KeyType::CtrlC => {
                self.branch_picker = None;
                self.status_message = Some("Branch picker cancelled".to_string());
            }
            KeyType::Runes if key.runes == ['q'] => {
                self.branch_picker = None;
            }
            _ => {} // consume all other input while picker is open
        }

        None
    }

    /// Switch the active branch to a different leaf. Reloads the conversation.
    fn switch_to_branch_leaf(&mut self, leaf_id: &str) -> bool {
        let Ok(session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return false;
        };
        let session_id = session_guard.header.id.clone();
        let old_leaf_id = session_guard.leaf_id.clone();
        drop(session_guard);

        let pending = PendingTreeNavigation {
            session_id,
            old_leaf_id,
            new_leaf_id: Some(leaf_id.to_string()),
            editor_text: None,
            entries_to_summarize: Vec::new(),
            summary_from_id: String::new(),
            api_key_present: false,
        };
        self.start_tree_navigation(pending, TreeSummaryChoice::NoSummary, None)
    }

    /// Open the branch picker if the session has sibling branches.
    pub fn open_branch_picker(&mut self) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch branches while processing".to_string());
            return;
        }

        let Ok(session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return;
        };
        let branches = session_guard.sibling_branches().map(|(_, b)| b);
        drop(session_guard);

        match branches {
            Some(branches) if branches.len() > 1 => {
                let mut picker = BranchPickerOverlay::new(branches);
                picker.max_visible = super::overlay_max_visible(self.term_height);
                self.branch_picker = Some(picker);
            }
            _ => {
                self.status_message =
                    Some("No branches to pick (use /fork to create one)".to_string());
            }
        }
    }

    /// Cycle to the next or previous sibling branch (Ctrl+Right / Ctrl+Left).
    pub fn cycle_sibling_branch(&mut self, forward: bool) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch branches while processing".to_string());
            return;
        }

        let Ok(session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return;
        };
        let target = session_guard.sibling_branches().and_then(|(_, branches)| {
            if branches.len() <= 1 {
                return None;
            }
            let current_idx = branches.iter().position(|b| b.is_current)?;
            let next_idx = if forward {
                (current_idx + 1) % branches.len()
            } else {
                current_idx.checked_sub(1).unwrap_or(branches.len() - 1)
            };
            Some(branches[next_idx].leaf_id.clone())
        });
        drop(session_guard);

        if let Some(leaf_id) = target {
            self.switch_to_branch_leaf(&leaf_id);
        } else {
            self.status_message = Some("No sibling branches (use /fork to create one)".to_string());
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn start_tree_navigation(
        &mut self,
        pending: PendingTreeNavigation,
        choice: TreeSummaryChoice,
        custom_instructions: Option<String>,
    ) -> bool {
        let summary_requested = matches!(
            choice,
            TreeSummaryChoice::Summarize | TreeSummaryChoice::SummarizeWithCustomPrompt
        );

        // Fast path: no summary + no extensions. Keep it synchronous so unit tests can drive it
        // without running the async runtime.
        if !summary_requested && self.extensions.is_none() {
            let Ok(mut session_guard) = self.session.try_lock() else {
                self.status_message = Some("Session busy; try again".to_string());
                return false;
            };

            if let Some(target_id) = &pending.new_leaf_id {
                if !session_guard.navigate_to(target_id) {
                    self.status_message = Some(format!("Branch target not found: {target_id}"));
                    return false;
                }
            } else {
                session_guard.reset_leaf();
            }

            let (messages, usage) = conversation_from_session(&session_guard);
            let agent_messages = session_guard.to_messages_for_current_path();
            let status_leaf = pending
                .new_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());
            drop(session_guard);

            if let Ok(mut agent_guard) = self.agent.try_lock() {
                agent_guard.replace_messages(agent_messages);
            }

            self.messages = messages;
            self.message_render_cache.clear();
            self.total_usage = usage;
            self.current_response.clear();
            self.current_thinking.clear();
            self.agent_state = AgentState::Idle;
            self.current_tool = None;
            self.abort_handle = None;
            self.status_message = Some(format!("Switched to {status_leaf}"));
            if let Err(message) = self.sync_runtime_selection_from_session_header() {
                self.status_message = Some(message);
            }
            self.spawn_save_session();
            self.scroll_to_bottom();

            if let Some(text) = pending.editor_text {
                self.input.set_value(&text);
            }
            self.input.focus();

            return true;
        }

        let event_tx = self.event_tx.clone();
        let session = Arc::clone(&self.session);
        let agent = Arc::clone(&self.agent);
        let extensions = self.extensions.clone();
        let reserve_tokens = self.config.branch_summary_reserve_tokens();
        let runtime_handle = self.runtime_handle.clone();

        let Ok(agent_guard) = self.agent.try_lock() else {
            self.status_message = Some("Agent busy; try again".to_string());
            self.agent_state = AgentState::Idle;
            return false;
        };
        let provider = agent_guard.provider();
        let key_opt = agent_guard.stream_options().api_key.clone();

        self.tree_ui = None;
        self.agent_state = AgentState::Processing;
        self.status_message = Some("Switching branches...".to_string());

        runtime_handle.spawn(async move {
            let cx = Cx::for_request();

            let from_id_for_event = pending
                .old_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());
            let to_id_for_event = pending
                .new_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());

            if let Some(manager) = extensions.clone() {
                let cancelled = manager
                    .dispatch_cancellable_event(
                        ExtensionEventName::SessionBeforeSwitch,
                        Some(json!({
                            "fromId": from_id_for_event.clone(),
                            "toId": to_id_for_event.clone(),
                            "sessionId": pending.session_id.clone(),
                        })),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(false);
                if cancelled {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::System("Session switch cancelled by extension".to_string()),
                    )
                    .await;
                    return;
                }
            }

            let summary_skipped =
                summary_requested && key_opt.is_none() && !pending.entries_to_summarize.is_empty();
            let summary_text = if !summary_requested || pending.entries_to_summarize.is_empty() {
                None
            } else if let Some(key) = key_opt.as_deref() {
                match crate::compaction::summarize_entries(
                    &pending.entries_to_summarize,
                    provider,
                    key,
                    reserve_tokens,
                    custom_instructions.as_deref(),
                )
                .await
                {
                    Ok(summary) => summary,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Branch summary failed: {err}")),
                        )
                        .await;
                        return;
                    }
                }
            } else {
                None
            };

            let mut summary_entry_payload: Option<Value> = None;
            let mut summary_entry_id: Option<String> = None;

            let messages_for_agent = {
                let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };

                if let Some(target_id) = &pending.new_leaf_id {
                    if !guard.navigate_to(target_id) {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                            PiMsg::AgentError(format!("Branch target not found: {target_id}")),
                        )
                        .await;
                        return;
                    }
                } else {
                    guard.reset_leaf();
                }

                if let Some(summary_text) = summary_text {
                    let summary_clone = summary_text.clone();
                    guard.append_branch_summary(
                        pending.summary_from_id.clone(),
                        summary_text,
                        None,
                        None,
                    );
                    summary_entry_id = guard.leaf_id.clone();
                    let mut summary_entry = serde_json::Map::new();
                    summary_entry.insert(
                        "type".to_string(),
                        Value::String("branch_summary".to_string()),
                    );
                    summary_entry.insert(
                        "fromId".to_string(),
                        Value::String(pending.summary_from_id.clone()),
                    );
                    summary_entry.insert("summary".to_string(), Value::String(summary_clone));
                    summary_entry.insert("fromHook".to_string(), Value::Bool(false));
                    summary_entry_payload = Some(Value::Object(summary_entry));
                }

                let _ = guard.save().await;
                guard.to_messages_for_current_path()
            };

            {
                let mut agent_guard = match OwnedMutexGuard::lock(Arc::clone(&agent), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                agent_guard.replace_messages(messages_for_agent);
            }

            let (messages, usage) = {
                let guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                conversation_from_session(&guard)
            };

            let status = if summary_skipped {
                Some(format!(
                    "Switched to {to_id_for_event} (no summary: missing API key)"
                ))
            } else {
                Some(format!("Switched to {to_id_for_event}"))
            };

            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::ConversationReset {
                    messages,
                    usage,
                    status,
                },
            )
            .await;

            if let Some(text) = pending.editor_text {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::SetEditorText(text),
                )
                .await;
            }

            if let Some(manager) = extensions {
                let new_leaf_id = summary_entry_id
                    .clone()
                    .or_else(|| pending.new_leaf_id.clone());
                let old_leaf_value = pending
                    .old_leaf_id
                    .clone()
                    .map_or(Value::Null, Value::String);
                let new_leaf_value = new_leaf_id.clone().map_or(Value::Null, Value::String);
                let mut tree_payload = serde_json::Map::new();
                tree_payload.insert("newLeafId".to_string(), new_leaf_value);
                tree_payload.insert("oldLeafId".to_string(), old_leaf_value);
                if let Some(summary_entry) = summary_entry_payload {
                    tree_payload.insert("summaryEntry".to_string(), summary_entry);
                }

                let _ = manager
                    .dispatch_event(
                        ExtensionEventName::SessionSwitch,
                        Some(json!({
                            "fromId": from_id_for_event,
                            "toId": to_id_for_event,
                            "sessionId": pending.session_id,
                        })),
                    )
                    .await;
                let _ = manager
                    .dispatch_event(
                        ExtensionEventName::SessionTree,
                        Some(Value::Object(tree_payload)),
                    )
                    .await;
            }
        });
        true
    }
}
