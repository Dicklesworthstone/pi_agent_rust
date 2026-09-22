//! Resolve SDK providers against the extension runtime the session actually owns.
//!
//! Extension registration requires an AgentSession, but choosing an extension
//! model requires registration to have completed. The provisional model is only
//! construction state: it is never installed in the persisted session header.

use super::{AgentSession, AuthStorage, Cli, Config, Error, ModelRegistry, Result};
use crate::app::{self, ModelSelection};
use crate::auth::OAuthRefreshReport;
use crate::models::{default_models_path, extension_provider_bindings};
use crate::provider::InputType;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub(super) fn provisional_selection(registry: &ModelRegistry) -> Result<ModelSelection> {
    let model_entry = app::bootstrap_model_entry(registry)
        .ok_or_else(|| Error::config("No built-in model is available to initialize extensions"))?;
    Ok(ModelSelection {
        thinking_level: model_entry.clamp_thinking_level(crate::model::ThinkingLevel::Off),
        model_entry,
        scoped_models: Vec::new(),
        fallback_message: None,
    })
}

pub(super) struct SelectionInputs<'a> {
    pub cli: &'a Cli,
    pub config: &'a Config,
    pub scoped_patterns: &'a [String],
    pub global_dir: &'a Path,
    pub oauth_refresh: &'a OAuthRefreshReport,
    pub preserve_compaction_window: bool,
}

/// Finish selection once, using the same runtime that loaded the extensions.
/// On failure, stop that runtime before returning the original startup error.
pub(super) async fn finish_selection(
    session: &mut AgentSession,
    registry: &mut ModelRegistry,
    auth: &mut AuthStorage,
    inputs: SelectionInputs<'_>,
) -> Result<()> {
    // Keep the initiating owner's context on every poll, even when an embedder
    // polls this future from a different task. Cleanup retains its existing
    // independent runtime shutdown budget after the operation has settled.
    let owner = crate::agent_cx::AgentCx::for_current_or_request();
    let result = owner
        .with_current(finish_selection_inner(session, registry, auth, inputs))
        .await;
    if result.is_err()
        && let Some(region) = session.extensions.as_ref()
        && !region.shutdown().await
    {
        tracing::warn!(
            target: "pi::sdk",
            "extension runtime did not stop within its budget after provider selection failed"
        );
    }
    result
}

#[allow(clippy::too_many_lines)]
async fn finish_selection_inner(
    session: &mut AgentSession,
    registry: &mut ModelRegistry,
    auth: &mut AuthStorage,
    inputs: SelectionInputs<'_>,
) -> Result<()> {
    let cx = crate::agent_cx::AgentCx::for_current_or_request();
    ensure_startup_active(&cx)?;
    let manager = session
        .extensions
        .as_ref()
        .map(|region| region.manager().clone())
        .ok_or_else(|| Error::extension("Extension registration did not produce a runtime"))?;
    let bindings = extension_provider_bindings(&manager.extension_providers())
        .map_err(|error| Error::validation(error.to_string()))?;
    let entries = manager.extension_model_entries();
    let explicit_key = inputs
        .cli
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty());

    // Native startup cannot refresh OAuth configurations that have not been
    // registered yet. An explicitly routed session must not contact unrelated
    // token endpoints at all, rather than merely ignoring their errors later.
    // Automatic/bare-model selection still refreshes candidates first: expired
    // OAuth credentials are not ready until refreshed, so selecting beforehand
    // could silently move a resumed/configured session to a different provider.
    let configs = bindings
        .iter()
        .filter_map(|binding| {
            binding
                .oauth_config
                .as_ref()
                .map(|config| (binding.provider.clone(), config.clone()))
        })
        .collect::<Vec<_>>();
    let refresh_failures = refresh_extension_credentials(
        auth,
        inputs.cli,
        &configs,
        &crate::http::client::Client::new(),
    )
    .await;
    ensure_startup_active(&cx)?;

    // Prepare a private registry. A failed selection or cancelled lock waiter
    // must not publish extension registrations or credentials into the caller's
    // registry while leaving its provider and stored identity unchanged.
    // OAuth renewal itself is independent credential maintenance; it is not
    // rolled back when model selection fails.
    // Provider-only bindings (including overrides of built-in transports) and
    // declared model rows must both participate in selection and later /model.
    let mut candidate_registry =
        ModelRegistry::load(auth, Some(default_models_path(inputs.global_dir)));
    candidate_registry
        .merge_extension_registry(&bindings, entries)
        .map_err(|error| Error::validation(error.to_string()))?;
    let scoped_models = if inputs.scoped_patterns.is_empty() {
        Vec::new()
    } else {
        app::resolve_model_scope(inputs.scoped_patterns, &candidate_registry, explicit_key)
    };
    let store = Arc::clone(&session.session);
    let selection = {
        let stored = store
            .lock(cx.cx())
            .await
            .map_err(|error| Error::session(error.to_string()))?;
        ensure_startup_active(&cx)?;
        app::select_model_and_thinking(
            inputs.cli,
            inputs.config,
            &stored,
            &candidate_registry,
            &scoped_models,
            inputs.global_dir,
        )
        .map_err(|error| Error::validation(error.to_string()))?
    };

    let selected_provider = &selection.model_entry.model.provider;
    if !explicit_key {
        if let Some((_, error)) = refresh_failures.iter().find(|(provider, _)| {
            crate::provider_metadata::provider_ids_match(provider, selected_provider)
        }) {
            return Err(Error::auth(format!(
                "OAuth token refresh failed for: {selected_provider} ({error})"
            )));
        }
        // An extension's explicit OAuth configuration supersedes a built-in
        // refresher for the same provider. Otherwise retain native startup's
        // selected-provider error behavior.
        if !bindings.iter().any(|binding| {
            crate::provider_metadata::provider_ids_match(&binding.provider, selected_provider)
                && binding.oauth_config.is_some()
        }) && let Some(failure) = inputs.oauth_refresh.failure_for(selected_provider)
        {
            return Err(Error::auth(format!(
                "OAuth token refresh failed for: {} ({})",
                failure.provider, failure.error
            )));
        }
    }
    let api_key = app::resolve_api_key(auth, inputs.cli, &selection.model_entry)
        .map_err(|error| Error::validation(error.to_string()))?;
    ensure_startup_active(&cx)?;
    let provider = crate::providers::create_provider(&selection.model_entry, Some(&manager))?;

    // Complete every fallible operation before publishing the final identity.
    // Keep the extension-installed request hook and other stream options; only
    // model-dependent settings change, just as on the classic startup path.
    let mut stored = store
        .lock(cx.cx())
        .await
        .map_err(|error| Error::session(error.to_string()))?;
    // No await or fallible operation separates this final admission check from
    // installation of provider/options, stored identity and both registries.
    ensure_startup_active(&cx)?;
    session.agent.set_provider(provider);
    session.agent.set_keyword_max_thinking_level(
        selection
            .model_entry
            .clamp_thinking_level(crate::model::ThinkingLevel::Max),
    );
    session
        .agent
        .set_tool_call_dialect(selection.model_entry.tool_call_dialect());
    session.agent.set_model_accepts_images(
        selection
            .model_entry
            .model
            .input
            .contains(&InputType::Image),
    );
    {
        let options = session.agent.stream_options_mut();
        options.api_key = api_key;
        options.headers.clone_from(&selection.model_entry.headers);
        options.thinking_level = Some(selection.thinking_level);
        options.max_tokens = Some(selection.model_entry.model.max_tokens);
    }
    if !inputs.preserve_compaction_window {
        let window = selection.model_entry.model.context_window;
        session.set_compaction_context_window(if window == 0 {
            crate::compaction::ResolvedCompactionSettings::default().context_window_tokens
        } else {
            window
        });
    }
    app::update_session_for_selection(&mut stored, &selection);
    drop(stored);
    session.set_model_registry(candidate_registry.clone());
    *registry = candidate_registry;
    session.set_auth_storage(auth.clone());
    manager.set_current_model(
        Some(selection.model_entry.model.provider),
        Some(selection.model_entry.model.id),
    );
    session.refresh_extension_completion_host_state();
    Ok(())
}

fn ensure_startup_active(owner: &crate::agent_cx::AgentCx) -> Result<()> {
    if owner.cx().is_cancel_requested() {
        return Err(Error::session(
            "SDK_EXTENSION_STARTUP_CANCELLED: provider selection owner was cancelled",
        ));
    }
    Ok(())
}

/// Match the same explicit-provider precedence as `app::select_model_and_thinking`.
/// A bare model can exist in several providers; do not guess its owner before
/// credentials are refreshed. The registered runtime identity stays unchanged.
fn refresh_matches_request(cli: &Cli, provider: &str) -> bool {
    let requested = cli.provider.as_deref().or_else(|| {
        cli.model
            .as_deref()
            .and_then(crate::provider_metadata::split_provider_model_spec)
            .map(|(provider, _)| provider)
    });
    requested.is_none_or(|requested| {
        crate::provider_metadata::provider_ids_match(requested, provider)
    })
}

async fn refresh_extension_credentials(
    auth: &mut AuthStorage,
    cli: &Cli,
    configs: &[(String, crate::models::OAuthConfig)],
    client: &crate::http::client::Client,
) -> HashMap<String, String> {
    let mut failures = HashMap::new();
    if cli.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
        return failures;
    }
    for (provider, config) in configs {
        if !refresh_matches_request(cli, provider) {
            continue;
        }
        let configs = HashMap::from([(provider.clone(), config.clone())]);
        if let Err(error) = auth
            .refresh_expired_extension_oauth_tokens(client, &configs)
            .await
        {
            failures.insert(provider.clone(), error.to_string());
        }
    }
    failures
}

#[cfg(test)]
#[path = "extension_bootstrap_tests.rs"]
mod tests;
