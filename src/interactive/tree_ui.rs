use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeNavigationPersistenceOutcome {
    Confirmed,
    Disabled,
}

#[derive(Debug)]
struct TreeNavigationCommit {
    messages_for_ui: Vec<ConversationMessage>,
    usage: crate::model::Usage,
    new_leaf_id: Option<String>,
    summary_entry_id: Option<String>,
    summary_entry_payload: Option<Value>,
    persistence: TreeNavigationPersistenceOutcome,
}

async fn stage_and_commit_tree_navigation(
    agent: &mut InteractiveAgent,
    expected_session_id: &str,
    expected_leaf_id: Option<&str>,
    target_leaf_id: Option<&str>,
    summary: Option<(String, String)>,
    cx: &Cx,
) -> crate::error::Result<TreeNavigationCommit> {
    let admission = agent.handle.session().provider_admission_gate();
    let _provider_authority = admission.acquire_transition_authority(cx).await?;
    let session_actions = agent.handle.session().session_action_admission_gate();
    let _session_action_permit = session_actions.acquire(cx).await?;
    let mut live = OwnedMutexGuard::lock(agent.handle.session_store(), cx)
        .await
        .map_err(|err| crate::error::Error::session(err.to_string()))?;
    if live.header.id != expected_session_id || live.leaf_id() != expected_leaf_id {
        return Err(crate::error::Error::session(
            "Session changed while switching branches; the switch was not applied".to_string(),
        ));
    }

    let mut candidate = live.clone();
    candidate.ensure_full_v2_hydration_before_save()?;
    if let Some(target_id) = target_leaf_id {
        if !candidate.navigate_to(target_id) {
            return Err(crate::error::Error::session(format!(
                "Branch target not found: {target_id}"
            )));
        }
    } else {
        candidate.reset_leaf();
    }

    let branch_changed = candidate.leaf_id() != live.leaf_id();
    let (summary_entry_payload, summary_entry_id) = if let Some((from_id, summary_text)) = summary {
        let summary_clone = summary_text.clone();
        let entry_id = candidate.append_branch_summary(from_id.clone(), summary_text, None, None);
        let mut summary_entry = serde_json::Map::new();
        summary_entry.insert(
            "type".to_string(),
            Value::String("branch_summary".to_string()),
        );
        summary_entry.insert("fromId".to_string(), Value::String(from_id));
        summary_entry.insert("summary".to_string(), Value::String(summary_clone));
        summary_entry.insert("fromHook".to_string(), Value::Bool(false));
        (Some(Value::Object(summary_entry)), Some(entry_id))
    } else {
        (None, None)
    };

    let persistence = if agent.handle.session().save_enabled() {
        TreeNavigationPersistenceOutcome::Confirmed
    } else {
        TreeNavigationPersistenceOutcome::Disabled
    };
    // Share the SDK's transaction: prepare removal of the departing plan pin,
    // persist the candidate, then publish its transcript and authority together.
    // An interrupted/failed save preserves the old view and fences later turns.
    agent
        .handle
        .session_mut()
        .install_context_candidate(&mut live, candidate, true, &admission, "tree navigation", cx)
        .await?;
    if branch_changed {
        agent.plans.clear_review();
        agent.plans.forget_model_restore();
    }
    let new_leaf_id = live.leaf_id().map(str::to_string);
    let (messages_for_ui, usage) = conversation_from_session(&live);
    Ok(TreeNavigationCommit {
        messages_for_ui,
        usage,
        new_leaf_id,
        summary_entry_id,
        summary_entry_payload,
        persistence,
    })
}

fn install_memory_tree_navigation(
    agent: &mut InteractiveAgent,
    live: &mut Session,
    expected_session_id: &str,
    expected_leaf_id: Option<&str>,
    target_leaf_id: Option<&str>,
) -> crate::error::Result<TreeNavigationCommit> {
    if live.header.id != expected_session_id || live.leaf_id() != expected_leaf_id {
        return Err(crate::error::Error::session(
            "Session changed while switching branches; the switch was not applied",
        ));
    }
    agent.handle.session().ensure_provider_reentry_allowed()?;
    let mut candidate = live.clone();
    candidate.ensure_full_v2_hydration_before_save()?;
    if let Some(target_id) = target_leaf_id {
        if !candidate.navigate_to(target_id) {
            return Err(crate::error::Error::session(format!(
                "Branch target not found: {target_id}"
            )));
        }
    } else {
        candidate.reset_leaf();
    }
    let prompt = if candidate.leaf_id() != live.leaf_id() {
        Some(crate::plan::replacement_system_prompt(
            agent,
            &agent.handle.session_store(),
            live,
        )?)
    } else {
        None
    };
    let (messages_for_ui, usage) = conversation_from_session(&candidate);
    let agent_messages = candidate.to_messages_for_current_path();
    let new_leaf_id = candidate.leaf_id().map(str::to_string);
    *live = candidate;
    agent.handle.session_mut().invalidate_background_compaction();
    agent.replace_messages(agent_messages);
    if let Some(prompt) = prompt {
        agent
            .plan_state()
            .reset_for_session(crate::plan::replayed_plan_mode(live));
        agent.set_system_prompt(prompt);
        agent.plans.clear_review();
        agent.plans.forget_model_restore();
    }
    Ok(TreeNavigationCommit {
        messages_for_ui,
        usage,
        new_leaf_id,
        summary_entry_id: None,
        summary_entry_payload: None,
        persistence: TreeNavigationPersistenceOutcome::Disabled,
    })
}

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

        // Fast path: in-memory only. Persistence-enabled switches must go
        // through the staged async path so a failed save cannot claim success.
        if !summary_requested && self.extensions.is_none() && !self.save_enabled {
            let Ok(mut agent_guard) = self.agent.try_lock() else {
                self.status_message = Some("Agent busy; try again".to_string());
                return false;
            };
            let Ok(mut session_guard) = self.session.try_lock() else {
                self.status_message = Some("Session busy; try again".to_string());
                return false;
            };
            let commit = match install_memory_tree_navigation(
                &mut agent_guard,
                &mut session_guard,
                &pending.session_id,
                pending.old_leaf_id.as_deref(),
                pending.new_leaf_id.as_deref(),
            ) {
                Ok(commit) => commit,
                Err(error) => {
                    self.status_message = Some(error.to_string());
                    return false;
                }
            };

            let status_leaf = pending
                .new_leaf_id
                .clone()
                .unwrap_or_else(|| "root".to_string());
            drop(session_guard);
            drop(agent_guard);

            self.messages = commit.messages_for_ui;
            self.message_render_cache.clear();
            self.total_usage = commit.usage;
            self.current_response.clear();
            self.current_thinking.clear();
            self.agent_state = AgentState::Idle;
            self.current_tool = None;
            self.abort_handle = None;
            self.status_message = Some(format!("Switched to {status_leaf}"));
            if let Err(message) = self.sync_runtime_selection_from_session_header() {
                self.status_message = Some(message);
            }
            self.scroll_to_bottom();

            if let Some(text) = pending.editor_text {
                self.input.set_value(&text);
            }
            self.input.focus();

            return true;
        }

        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let extensions = self.extensions.clone();
        let reserve_tokens = self.config.branch_summary_reserve_tokens();
        let runtime_handle = self.runtime_handle.clone();

        let Ok(agent_guard) = self.agent.try_lock() else {
            self.status_message = Some("Agent busy; try again".to_string());
            self.agent_state = AgentState::Idle;
            return false;
        };
        // Summary generation calls the provider before the candidate installer.
        // A prior uncertain save must fence that request as well.
        if let Err(error) = agent_guard.handle.session().ensure_provider_reentry_allowed() {
            self.status_message = Some(format!("Branch switch is blocked: {error}"));
            return false;
        }
        let provider = agent_guard.provider();
        let stream_options = agent_guard.stream_options().clone();
        let privacy = agent_guard.compaction_privacy();
        drop(agent_guard);

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

            let summary_text = if !summary_requested || pending.entries_to_summarize.is_empty() {
                None
            } else {
                match crate::compaction::summarize_entries(
                    &pending.entries_to_summarize,
                    provider,
                    &stream_options,
                    reserve_tokens,
                    custom_instructions.as_deref(),
                    &privacy,
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
            };

            // Keep Agent and Session branch histories atomic. Acquiring Agent
            // first matches every full-session transition; no Session bytes
            // change if the second lock cannot be acquired.
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
            let expected_leaf_id = pending.old_leaf_id.clone();
            let summary = summary_text.map(|text| (pending.summary_from_id.clone(), text));
            let commit = match stage_and_commit_tree_navigation(
                &mut agent_guard,
                &pending.session_id,
                expected_leaf_id.as_deref(),
                pending.new_leaf_id.as_deref(),
                summary,
                &cx,
            )
            .await
            {
                Ok(commit) => commit,
                Err(err) => {
                    drop(agent_guard);
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &cx,
                        PiMsg::AgentError(format!("Branch switch could not be confirmed: {err}")),
                    )
                    .await;
                    return;
                }
            };
            let TreeNavigationCommit {
                messages_for_ui: messages,
                usage,
                new_leaf_id,
                summary_entry_id,
                summary_entry_payload,
                persistence,
            } = commit;
            drop(agent_guard);

            let switched_to = new_leaf_id
                .clone()
                .unwrap_or_else(|| to_id_for_event.clone());
            let status = match persistence {
                TreeNavigationPersistenceOutcome::Confirmed => {
                    Some(format!("Switched to {switched_to}"))
                }
                TreeNavigationPersistenceOutcome::Disabled => {
                    Some(format!("Switched to {switched_to} (memory only)"))
                }
            };

            let delivered = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::ConversationReset {
                    session_id: pending.session_id.clone(),
                    messages,
                    usage,
                    status,
                },
            )
            .await;

            if delivered && let Some(text) = pending.editor_text {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::SetEditorText {
                        owner_session_id: pending.session_id.clone(),
                        text,
                    },
                )
                .await;
            }

            if delivered && let Some(manager) = extensions {
                let event_leaf_id = summary_entry_id
                    .clone()
                    .or(new_leaf_id)
                    .or_else(|| pending.new_leaf_id.clone());
                let old_leaf_value = pending
                    .old_leaf_id
                    .clone()
                    .map_or(Value::Null, Value::String);
                let new_leaf_value = event_leaf_id.map_or(Value::Null, Value::String);
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

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use std::sync::OnceLock;
    use tempfile::TempDir;

    fn runtime() -> &'static asupersync::runtime::Runtime {
        static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            RuntimeBuilder::multi_thread()
                .blocking_threads(1, 4)
                .build()
                .expect("build runtime")
        })
    }

    fn test_agent(session: Arc<Mutex<Session>>, save_enabled: bool) -> InteractiveAgent {
        let provider = Arc::new(
            crate::providers::openai::OpenAIProvider::new("classic-plan-branch")
                .with_base_url("http://127.0.0.1:1/v1"),
        );
        let mut agent = Agent::new(
            provider,
            crate::tools::ToolRegistry::new(&[], std::path::Path::new("."), None),
            crate::agent::AgentConfig {
                system_prompt: Some("Independent instructions".to_string()),
                ..crate::agent::AgentConfig::default()
            },
        );
        agent.replace_messages(session.try_lock().unwrap().to_messages_for_current_path());
        let session = crate::agent::AgentSession::new(
            agent,
            session,
            save_enabled,
            crate::compaction::ResolvedCompactionSettings::default(),
        );
        InteractiveAgent {
            handle: crate::sdk::AgentSessionHandle::from_session_with_listeners(
                session,
                crate::sdk::EventListeners::default(),
            ),
            plans: Default::default(),
        }
    }

    fn approve_plan(agent: &mut InteractiveAgent) -> crate::plan::SessionPlanReview {
        let owner = crate::agent_cx::AgentCx::for_request();
        let entered = runtime().block_on(agent.handle.enter_plan_mode(&owner)).unwrap();
        assert!(!matches!(
            entered.persistence,
            crate::plan::PlanPersistence::Unconfirmed { .. }
        ));
        assert!(agent.plan_state().submit_plan("Inspect then implement".to_string()));
        let review = agent.handle.pending_plan_review().unwrap().unwrap();
        let approved = runtime()
            .block_on(agent.handle.approve_plan_review(&owner, &review))
            .unwrap();
        assert!(!matches!(
            approved.persistence,
            crate::plan::PlanPersistence::Unconfirmed { .. }
        ));
        review
    }

    fn branched_session(session_dir: Option<std::path::PathBuf>) -> (Session, String, String) {
        let mut session = session_dir.map_or_else(Session::in_memory, |dir| {
            Session::create_with_dir(Some(dir))
        });
        let root_id = session.append_message(crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text("root".to_string()),
            timestamp: Some(0),
        });
        let current_leaf_id = session.append_message(crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text("current".to_string()),
            timestamp: Some(0),
        });
        assert!(session.create_branch_from(&root_id));
        let target_leaf_id = session.append_message(crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text("target".to_string()),
            timestamp: Some(0),
        });
        assert!(session.navigate_to(&current_leaf_id));
        (session, current_leaf_id, target_leaf_id)
    }

    #[test]
    fn staged_tree_navigation_save_failure_leaves_live_session_exact() {
        let temp = TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");
        let (mut raw_session, current_leaf_id, target_leaf_id) =
            branched_session(Some(temp.path().join("sessions")));
        raw_session.path = Some(blocked_path);
        let expected_session_id = raw_session.header.id.clone();
        let expected_entries =
            serde_json::to_value(&raw_session.entries).expect("serialize entries");
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), true);
        let cx = Cx::for_testing();

        let error = runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                &expected_session_id,
                Some(&current_leaf_id),
                Some(&target_leaf_id),
                None,
                &cx,
            ))
            .expect_err("directory session path must reject tree navigation save");
        assert!(!error.to_string().is_empty());

        runtime().block_on(async {
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock unchanged session");
            assert_eq!(guard.header.id, expected_session_id);
            assert_eq!(guard.leaf_id(), Some(current_leaf_id.as_str()));
            assert_eq!(
                serde_json::to_value(&guard.entries).expect("serialize live entries"),
                expected_entries
            );
        });
    }

    #[test]
    fn staged_tree_navigation_with_saving_disabled_commits_only_in_memory() {
        let temp = TempDir::new().expect("tempdir");
        let session_dir = temp.path().join("sessions");
        let (raw_session, current_leaf_id, target_leaf_id) = branched_session(None);
        let expected_session_id = raw_session.header.id.clone();
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), false);
        let cx = Cx::for_testing();

        let commit = runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                &expected_session_id,
                Some(&current_leaf_id),
                Some(&target_leaf_id),
                None,
                &cx,
            ))
            .expect("memory-only tree navigation");
        assert_eq!(commit.new_leaf_id.as_deref(), Some(target_leaf_id.as_str()));
        assert_eq!(
            commit.persistence,
            TreeNavigationPersistenceOutcome::Disabled
        );

        runtime().block_on(async {
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock memory-only session");
            assert!(guard.path.is_none());
            assert_eq!(guard.leaf_id(), Some(target_leaf_id.as_str()));
        });
        assert!(
            !session_dir.exists(),
            "--no-session tree navigation must not create durable state"
        );
    }

    #[test]
    fn staged_tree_navigation_success_reopens_at_target_leaf() {
        let temp = tempfile::Builder::new()
            .prefix("pi-tree-nav-")
            .tempdir_in("/tmp")
            .unwrap_or_else(|_| TempDir::new().expect("tempdir"));
        let (mut raw_session, current_leaf_id, target_leaf_id) =
            branched_session(Some(temp.path().to_path_buf()));
        raw_session.path = Some(temp.path().join("session.jsonl"));
        runtime()
            .block_on(raw_session.save())
            .unwrap_or_else(|err| panic!("baseline save must pin a path: {err}"));
        let expected_session_id = raw_session.header.id.clone();
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), true);
        let cx = Cx::for_testing();

        let persisted_path = runtime().block_on(async {
            let commit = stage_and_commit_tree_navigation(
                &mut agent,
                &expected_session_id,
                Some(&current_leaf_id),
                Some(&target_leaf_id),
                None,
                &cx,
            )
            .await
            .expect("durable tree navigation");
            assert_eq!(
                commit.persistence,
                TreeNavigationPersistenceOutcome::Confirmed
            );
            assert_eq!(commit.new_leaf_id.as_deref(), Some(target_leaf_id.as_str()));
            OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock saved session")
                .path
                .clone()
                .expect("saved path")
        });

        let reopened = runtime()
            .block_on(Session::open(persisted_path.to_string_lossy().as_ref()))
            .expect("reopen switched session");
        assert_eq!(reopened.leaf_id(), Some(target_leaf_id.as_str()));
    }

    #[test]
    fn staged_tree_navigation_rejects_stale_session_identity_before_mutation() {
        let (raw_session, current_leaf_id, target_leaf_id) = branched_session(None);
        let live_leaf = raw_session.leaf_id().map(str::to_string);
        let expected_entries =
            serde_json::to_value(&raw_session.entries).expect("serialize entries");
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), false);
        let cx = Cx::for_testing();

        let error = runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                "replaced-session-id",
                Some(&current_leaf_id),
                Some(&target_leaf_id),
                None,
                &cx,
            ))
            .expect_err("stale tree navigation must fail closed");
        assert!(error.to_string().contains("Session changed"));

        runtime().block_on(async {
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock rejected session");
            assert_eq!(guard.leaf_id(), live_leaf.as_deref());
            assert_eq!(
                serde_json::to_value(&guard.entries).expect("serialize live entries"),
                expected_entries
            );
        });
    }

    #[test]
    fn staged_tree_navigation_rejects_stale_leaf_before_mutation() {
        let (raw_session, _current_leaf_id, target_leaf_id) = branched_session(None);
        let session_id = raw_session.header.id.clone();
        let live_leaf = raw_session.leaf_id().expect("live leaf").to_string();
        let expected_entries =
            serde_json::to_value(&raw_session.entries).expect("serialize entries");
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), false);
        let cx = Cx::for_testing();

        let error = runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                &session_id,
                None,
                Some(&target_leaf_id),
                None,
                &cx,
            ))
            .expect_err("stale leaf must fail closed");
        assert!(error.to_string().contains("Session changed"));

        runtime().block_on(async {
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock rejected session");
            assert_eq!(guard.leaf_id(), Some(live_leaf.as_str()));
            assert_eq!(
                serde_json::to_value(&guard.entries).expect("serialize live entries"),
                expected_entries
            );
        });
    }

    #[test]
    fn staged_tree_navigation_retires_approval_and_only_its_owned_prompt() {
        let (raw_session, _, target) = branched_session(None);
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), false);
        let old_review = approve_plan(&mut agent);
        let pinned = agent.system_prompt().unwrap().to_string();
        agent.set_system_prompt(Some(format!("prefix\n{pinned}\nsuffix")));
        let (id, leaf) = {
            let live = session.try_lock().unwrap();
            (live.header.id.clone(), live.leaf_id().map(str::to_string))
        };

        runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                &id,
                leaf.as_deref(),
                Some(&target),
                None,
                &Cx::for_testing(),
            ))
            .unwrap();
        assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::Off);
        assert_eq!(
            agent.system_prompt(),
            Some("prefix\nIndependent instructions\nsuffix")
        );
        let owner = crate::agent_cx::AgentCx::for_request();
        let _ = runtime().block_on(agent.handle.enter_plan_mode(&owner)).unwrap();
        assert!(agent.plan_state().submit_plan(old_review.text().to_string()));
        let current = agent.handle.pending_plan_review().unwrap().unwrap();
        assert!(!old_review.same_submission(&current));
        assert!(runtime()
            .block_on(agent.handle.approve_plan_review(&owner, &old_review))
            .is_err());
        assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::PendingApproval);
    }

    #[test]
    fn staged_tree_noop_and_summary_append_preserve_live_approval() {
        let (raw_session, _, _) = branched_session(None);
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), false);
        let _ = approve_plan(&mut agent);
        let prompt = agent.system_prompt().map(str::to_string);
        for append_summary in [false, true] {
            let (id, leaf) = {
                let live = session.try_lock().unwrap();
                (live.header.id.clone(), live.leaf_id().map(str::to_string))
            };
            let summary = append_summary.then(|| {
                (leaf.clone().unwrap(), "Same-branch continuation".to_string())
            });
            runtime()
                .block_on(stage_and_commit_tree_navigation(
                    &mut agent,
                    &id,
                    leaf.as_deref(),
                    leaf.as_deref(),
                    summary,
                    &Cx::for_testing(),
                ))
                .unwrap();
            assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::Approved);
            assert_eq!(agent.system_prompt(), prompt.as_deref());
        }
    }

    #[test]
    fn staged_tree_failed_save_preserves_plan_but_fences_provider_reentry() {
        let temp = TempDir::new().unwrap();
        let (mut raw_session, _, target) =
            branched_session(Some(temp.path().join("sessions")));
        raw_session.path = Some(temp.path().join("source.jsonl"));
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), true);
        let _ = approve_plan(&mut agent);
        let prompt = agent.system_prompt().map(str::to_string);
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).unwrap();
        let (id, leaf, entries) = {
            let mut live = session.try_lock().unwrap();
            live.path = Some(blocked_path);
            (
                live.header.id.clone(),
                live.leaf_id().map(str::to_string),
                serde_json::to_value(&live.entries).unwrap(),
            )
        };
        let error = runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                &id,
                leaf.as_deref(),
                Some(&target),
                None,
                &Cx::for_testing(),
            ))
            .unwrap_err();
        assert!(error.is_session_persistence());
        assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::Approved);
        assert_eq!(agent.system_prompt(), prompt.as_deref());
        assert!(agent.handle.session().ensure_provider_reentry_allowed().is_err());
        let live = session.try_lock().unwrap();
        assert_eq!(live.leaf_id(), leaf.as_deref());
        assert_eq!(serde_json::to_value(&live.entries).unwrap(), entries);
    }

    #[test]
    fn staged_tree_rejects_ambiguous_owned_prompt_before_saving() {
        let (raw_session, _, target) = branched_session(None);
        let session = Arc::new(Mutex::new(raw_session));
        let mut agent = test_agent(Arc::clone(&session), false);
        let _ = approve_plan(&mut agent);
        let pinned = agent.system_prompt().unwrap().to_string();
        let duplicated = format!("{pinned}\n{pinned}");
        agent.set_system_prompt(Some(duplicated.clone()));
        let (id, leaf) = {
            let live = session.try_lock().unwrap();
            (live.header.id.clone(), live.leaf_id().map(str::to_string))
        };
        let error = runtime()
            .block_on(stage_and_commit_tree_navigation(
                &mut agent,
                &id,
                leaf.as_deref(),
                Some(&target),
                None,
                &Cx::for_testing(),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("PLAN_CONTEXT_CHANGED"));
        assert_eq!(session.try_lock().unwrap().leaf_id(), leaf.as_deref());
        assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::Approved);
        assert_eq!(agent.system_prompt(), Some(duplicated.as_str()));
        assert!(agent.handle.session().ensure_provider_reentry_allowed().is_ok());
    }

    #[test]
    fn memory_tree_navigation_preserves_noop_and_retires_departing_plan() {
        for approved in [false, true] {
            let (raw_session, _, target) = branched_session(None);
            let session = Arc::new(Mutex::new(raw_session));
            let mut agent = test_agent(Arc::clone(&session), false);
            let owner = crate::agent_cx::AgentCx::for_request();
            let _ = runtime().block_on(agent.handle.enter_plan_mode(&owner)).unwrap();
            assert!(agent.plan_state().submit_plan("Inspect then implement".to_string()));
            let review = agent.handle.pending_plan_review().unwrap().unwrap();
            if approved {
                let _ = runtime()
                    .block_on(agent.handle.approve_plan_review(&owner, &review))
                    .unwrap();
            }
            let mode = agent.plan_state().mode();
            let prompt = agent.system_prompt().map(str::to_string);
            let mut live = session.try_lock().unwrap();
            let id = live.header.id.clone();
            let leaf = live.leaf_id().map(str::to_string);
            install_memory_tree_navigation(
                &mut agent,
                &mut live,
                &id,
                leaf.as_deref(),
                leaf.as_deref(),
            )
            .unwrap();
            assert_eq!(agent.plan_state().mode(), mode);
            assert_eq!(agent.system_prompt(), prompt.as_deref());
            assert!(install_memory_tree_navigation(
                &mut agent,
                &mut live,
                &id,
                leaf.as_deref(),
                Some("missing-entry"),
            )
            .is_err());
            assert_eq!(agent.plan_state().mode(), mode);
            assert_eq!(agent.system_prompt(), prompt.as_deref());
            install_memory_tree_navigation(
                &mut agent,
                &mut live,
                &id,
                leaf.as_deref(),
                Some(&target),
            )
            .unwrap();
            drop(live);
            assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::Off);
            assert_eq!(agent.system_prompt(), Some("Independent instructions"));
            assert!(runtime()
                .block_on(agent.handle.approve_plan_review(&owner, &review))
                .is_err());
        }
    }
}
