//! UI-independent `/login` flow shared by the classic and FTUI stacks.
//!
//! Starting a login picks the provider's flow (API-key paste, browser OAuth
//! with an optional localhost callback, or RFC 8628 device flow) and returns
//! the instructions to show plus the pending state. Completing it exchanges
//! the user's code, key or device approval for a credential and saves it to
//! `auth.json`. The stacks own only the display and the input capture.

use std::path::Path;

use crate::auth::{AuthCredential, AuthStorage, DeviceFlowPollResult, OAuthCallbackServer};
use crate::error::Error;
use crate::extensions::ExtensionManager;
use crate::models::ModelEntry;

use super::commands::{
    api_key_login_prompt, extension_oauth_config_for_provider, format_login_provider_listing,
    normalize_api_key_input, normalize_auth_provider_input, registered_extension_provider_bindings,
    remove_provider_credentials, save_provider_credential, should_use_copilot_device_flow,
};
use super::state::{PendingLoginKind, PendingOAuth};

/// A login waiting for the user's next input. Opaque outside this module so
/// the stacks cannot tamper with the verifier or device code.
#[derive(Debug, Clone)]
pub struct PendingLogin(Box<PendingOAuth>);

impl PendingLogin {
    fn new(pending: PendingOAuth) -> Self {
        Self(Box::new(pending))
    }

    pub fn provider(&self) -> &str {
        &self.0.provider
    }

    /// Device flows complete on an empty submission (the user just confirms
    /// they approved in the browser); the other flows need a code or key.
    pub const fn accepts_empty_input(&self) -> bool {
        matches!(self.0.kind, PendingLoginKind::DeviceFlow)
    }
}

/// What `/login [provider]` produced.
pub enum LoginStart {
    /// `/login` with no provider: the provider/status listing.
    Listing(String),
    /// A flow now waits for input. `callback`, when present, delivers the
    /// browser redirect path by itself; manual paste still works.
    Pending {
        pending: PendingLogin,
        message: String,
        callback: Option<OAuthCallbackServer>,
    },
}

/// Why a completion did not save a credential.
pub enum LoginFailure {
    /// A device flow the user has not approved yet; the same pending login
    /// can be submitted again.
    StillPending(PendingLogin, String),
    /// Terminal: the user must run `/login` again.
    Failed(String),
}

fn device_flow_message(
    provider: &str,
    user_code: &str,
    verification_uri: &str,
    expires_in: u64,
) -> String {
    format!(
        "OAuth login: {provider}\n\n\
Open this URL:\n{verification_uri}\n\n\
If prompted, enter this code: {user_code}\n\
Code expires in {expires_in} seconds.\n\n\
After approving access in the browser, press Enter in Pi to complete login."
    )
}

fn copilot_config() -> crate::auth::CopilotOAuthConfig {
    crate::auth::CopilotOAuthConfig {
        client_id: crate::auth::resolved_copilot_client_id(),
        ..crate::auth::CopilotOAuthConfig::default()
    }
}

fn gitlab_config() -> crate::auth::GitLabOAuthConfig {
    crate::auth::GitLabOAuthConfig {
        client_id: std::env::var("GITLAB_CLIENT_ID").unwrap_or_default(),
        base_url: std::env::var("GITLAB_BASE_URL")
            .unwrap_or_else(|_| "https://gitlab.com".to_string()),
        ..crate::auth::GitLabOAuthConfig::default()
    }
}

/// Start `/login [args]`. Errors are user-facing messages.
#[allow(clippy::too_many_lines)]
pub async fn start_login(
    args: &str,
    auth_path: &Path,
    available_models: &[ModelEntry],
    extensions: Option<&ExtensionManager>,
) -> Result<LoginStart, String> {
    let args = args.trim();
    let bindings = registered_extension_provider_bindings(extensions)
        .map_err(|err| format!("Unable to load extension login providers: {err}"))?;
    if args.is_empty() {
        let auth = AuthStorage::load(auth_path.to_path_buf())
            .map_err(|err| format!("Unable to load auth status: {err}"))?;
        return Ok(LoginStart::Listing(format_login_provider_listing(
            &auth,
            available_models,
            &bindings,
        )));
    }

    let requested = args.split_whitespace().next().unwrap_or(args);
    let provider = normalize_auth_provider_input(requested);

    let device = if provider == "kimi-for-coding" {
        Some(crate::auth::start_kimi_code_device_flow().await)
    } else if (provider == "github-copilot" || provider == "copilot")
        && should_use_copilot_device_flow()
    {
        Some(crate::auth::start_copilot_device_flow(&copilot_config()).await)
    } else {
        None
    };
    if let Some(device) = device {
        let device = device.map_err(|err| format!("OAuth login failed: {err}"))?;
        let verification_uri = device
            .verification_uri_complete
            .unwrap_or(device.verification_uri);
        let message = device_flow_message(
            &provider,
            &device.user_code,
            &verification_uri,
            device.expires_in,
        );
        return Ok(LoginStart::Pending {
            pending: PendingLogin::new(PendingOAuth {
                provider,
                kind: PendingLoginKind::DeviceFlow,
                verifier: String::new(),
                oauth_config: None,
                device_code: Some(device.device_code),
                redirect_uri: None,
            }),
            message,
            callback: None,
        });
    }

    if let Some(prompt) = api_key_login_prompt(&provider) {
        return Ok(LoginStart::Pending {
            pending: PendingLogin::new(PendingOAuth {
                provider,
                kind: PendingLoginKind::ApiKey,
                verifier: String::new(),
                oauth_config: None,
                device_code: None,
                redirect_uri: None,
            }),
            message: prompt,
            callback: None,
        });
    }

    let (info, ext_config) = match provider.as_str() {
        "anthropic" => (crate::auth::start_anthropic_oauth(), None),
        "openai-codex" => (crate::auth::start_openai_codex_oauth(), None),
        "google-gemini-cli" => (crate::auth::start_google_gemini_cli_oauth(), None),
        "google-antigravity" => (crate::auth::start_google_antigravity_oauth(), None),
        "github-copilot" | "copilot" => (
            crate::auth::start_copilot_browser_oauth(&copilot_config()),
            None,
        ),
        "gitlab" | "gitlab-duo" => (crate::auth::start_gitlab_oauth(&gitlab_config()), None),
        _ => {
            let config =
                extension_oauth_config_for_provider(available_models, &bindings, &provider)
                    .ok_or_else(|| {
                        format!(
                            "Login not supported for {provider} (no built-in flow or OAuth config)"
                        )
                    })?;
            (
                crate::auth::start_extension_oauth(&provider, &config),
                Some(config),
            )
        }
    };
    let info = info.map_err(|err| format!("OAuth login failed: {err}"))?;

    // Use the pre-bound callback server when the provider created one
    // (Copilot/GitLab with a random port); otherwise start one for localhost
    // redirect URIs (issue #22).
    let callback = info.callback_server.or_else(|| {
        info.redirect_uri
            .as_deref()
            .filter(|uri| crate::auth::redirect_uri_needs_callback_server(uri))
            .and_then(|uri| crate::auth::start_oauth_callback_server(uri).ok())
    });

    let mut message = format!(
        "OAuth login: {}\n\nOpen this URL:\n{}\n",
        info.provider, info.url
    );
    if info.provider == "anthropic" {
        message.push_str(
            "\nWARNING: Anthropic OAuth (Claude Code consumer account) is no longer recommended.\n\
Using consumer OAuth tokens outside the official client may violate Anthropic's consumer Terms of Service and can\n\
result in account suspension/ban. Prefer using an Anthropic API key (ANTHROPIC_API_KEY) instead.\n",
        );
    }
    if callback.is_some() {
        message.push_str(
            "\nListening for callback — complete authorization in your browser.\n\
             Pi will continue automatically, or you can paste the code manually.",
        );
    } else {
        if let Some(instructions) = info.instructions {
            message.push('\n');
            message.push_str(&instructions);
            message.push('\n');
        }
        message.push_str("\nPaste the callback URL or authorization code into Pi to continue.");
    }

    Ok(LoginStart::Pending {
        pending: PendingLogin::new(PendingOAuth {
            provider: info.provider,
            kind: PendingLoginKind::OAuth,
            verifier: info.verifier,
            oauth_config: ext_config,
            device_code: None,
            redirect_uri: info.redirect_uri,
        }),
        message,
        callback,
    })
}

/// Exchange the user's input for a credential. Does not save it.
#[allow(clippy::too_many_lines)]
pub(super) async fn obtain_credential(
    pending: &PendingOAuth,
    code_input: &str,
) -> Result<AuthCredential, (bool, Error)> {
    let provider = pending.provider.as_str();
    let terminal = |err: Error| (false, err);
    match pending.kind {
        PendingLoginKind::ApiKey => normalize_api_key_input(code_input)
            .map(|key| AuthCredential::ApiKey { key })
            .map_err(|err| terminal(Error::auth(err))),
        PendingLoginKind::OAuth => {
            let verifier = pending.verifier.as_str();
            let result = match provider {
                "anthropic" => {
                    Box::pin(crate::auth::complete_anthropic_oauth(code_input, verifier)).await
                }
                "openai-codex" => {
                    Box::pin(crate::auth::complete_openai_codex_oauth(
                        code_input, verifier,
                    ))
                    .await
                }
                "google-gemini-cli" => {
                    Box::pin(crate::auth::complete_google_gemini_cli_oauth(
                        code_input, verifier,
                    ))
                    .await
                }
                "google-antigravity" => {
                    Box::pin(crate::auth::complete_google_antigravity_oauth(
                        code_input, verifier,
                    ))
                    .await
                }
                "github-copilot" | "copilot" => {
                    Box::pin(crate::auth::complete_copilot_browser_oauth(
                        &copilot_config(),
                        code_input,
                        verifier,
                        pending.redirect_uri.as_deref(),
                    ))
                    .await
                }
                "gitlab" | "gitlab-duo" => {
                    let redirect_uri = pending.redirect_uri.clone().or_else(|| {
                        pending
                            .oauth_config
                            .as_ref()
                            .and_then(|c| c.redirect_uri.clone())
                    });
                    Box::pin(crate::auth::complete_gitlab_oauth(
                        &gitlab_config(),
                        code_input,
                        verifier,
                        redirect_uri.as_deref(),
                    ))
                    .await
                }
                _ => match &pending.oauth_config {
                    Some(config) => {
                        Box::pin(crate::auth::complete_extension_oauth(
                            config, code_input, verifier,
                        ))
                        .await
                    }
                    None => Err(Error::auth(format!(
                        "OAuth provider not supported: {provider}"
                    ))),
                },
            };
            result.map_err(terminal)
        }
        PendingLoginKind::DeviceFlow => {
            let Some(device_code) = pending.device_code.as_deref() else {
                return Err(terminal(Error::auth(
                    "Device flow missing device_code".to_string(),
                )));
            };
            let poll = if provider == "kimi-for-coding" {
                Box::pin(crate::auth::poll_kimi_code_device_flow(device_code)).await
            } else if provider == "github-copilot" || provider == "copilot" {
                Box::pin(crate::auth::poll_copilot_device_flow(
                    &copilot_config(),
                    device_code,
                ))
                .await
            } else {
                DeviceFlowPollResult::Error(format!(
                    "Device flow polling not supported for {provider}"
                ))
            };
            match poll {
                DeviceFlowPollResult::Success(credential) => Ok(credential),
                DeviceFlowPollResult::Error(err) => Err(terminal(Error::auth(err))),
                DeviceFlowPollResult::Expired => Err(terminal(Error::auth(format!(
                    "Device code expired for {provider}. Run /login {provider} again."
                )))),
                DeviceFlowPollResult::AccessDenied => Err(terminal(Error::auth(format!(
                    "Access denied for {provider}."
                )))),
                DeviceFlowPollResult::Pending => Err((
                    true,
                    Error::auth(format!(
                        "Authorization for {provider} is still pending. Complete the browser step and submit again."
                    )),
                )),
                DeviceFlowPollResult::SlowDown => Err((
                    true,
                    Error::auth(format!(
                        "Authorization server asked to slow down for {provider}. Wait a few seconds and submit again."
                    )),
                )),
            }
        }
    }
}

/// Save `credential` for `provider` in `auth.json`.
pub(super) async fn save_credential(
    auth_path: &Path,
    provider: &str,
    credential: AuthCredential,
) -> crate::error::Result<()> {
    let mut auth = AuthStorage::load_async(auth_path.to_path_buf()).await?;
    save_provider_credential(&mut auth, provider, credential);
    auth.save_async().await
}

/// The success line both stacks show.
pub(super) fn success_status(provider: &str, kind: PendingLoginKind) -> String {
    match kind {
        PendingLoginKind::ApiKey => {
            format!("API key saved for {provider}. Credentials saved to auth.json.")
        }
        PendingLoginKind::OAuth | PendingLoginKind::DeviceFlow => {
            format!("OAuth login successful for {provider}. Credentials saved to auth.json.")
        }
    }
}

/// Complete a pending login and save the credential. On success returns the
/// provider and the status line to show.
pub async fn complete_login(
    pending: PendingLogin,
    code_input: &str,
    auth_path: &Path,
) -> Result<(String, String), LoginFailure> {
    let PendingLogin(inner) = pending;
    let credential = match obtain_credential(&inner, code_input).await {
        Ok(credential) => credential,
        Err((true, err)) => {
            return Err(LoginFailure::StillPending(
                PendingLogin(inner),
                err.to_string(),
            ));
        }
        Err((false, err)) => return Err(LoginFailure::Failed(err.to_string())),
    };
    save_credential(auth_path, &inner.provider, credential)
        .await
        .map_err(|err| LoginFailure::Failed(err.to_string()))?;
    let status = success_status(&inner.provider, inner.kind);
    Ok((inner.provider.clone(), status))
}

/// `/logout [provider]`: remove the stored credentials for `provider` (or the
/// active one). Returns the canonical provider and the status line.
pub fn logout(
    args: &str,
    active_provider: &str,
    auth_path: &Path,
) -> crate::error::Result<(String, String)> {
    let requested = args
        .split_whitespace()
        .next()
        .unwrap_or(active_provider)
        .trim()
        .to_ascii_lowercase();
    let provider = normalize_auth_provider_input(&requested);
    let mut auth = AuthStorage::load(auth_path.to_path_buf())?;
    let removed = remove_provider_credentials(&mut auth, &requested);
    auth.save()?;
    let status = if removed {
        format!("Removed stored credentials for {provider}.")
    } else {
        format!("No stored credentials for {provider}.")
    };
    Ok((provider, status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn api_key_login_saves_the_key_and_logout_removes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");

        let start = run(start_login("openai", &auth_path, &[], None)).expect("start");
        let LoginStart::Pending {
            pending,
            message,
            callback,
        } = start
        else {
            panic!("openai must start an API-key login");
        };
        assert!(message.contains("API key login: openai"), "{message}");
        assert!(callback.is_none());
        assert!(!pending.accepts_empty_input());

        let (provider, status) = run(complete_login(pending, "  sk-test-123  ", &auth_path))
            .unwrap_or_else(|_| {
                panic!("API-key completion must save");
            });
        assert_eq!(provider, "openai");
        assert!(status.starts_with("API key saved for openai"), "{status}");
        // `get`, not `resolve_api_key`: an OPENAI_API_KEY in the test
        // environment must not stand in for the stored credential.
        let auth = AuthStorage::load(auth_path.clone()).expect("reload auth");
        assert!(
            matches!(
                auth.get("openai"),
                Some(AuthCredential::ApiKey { key }) if key == "sk-test-123"
            ),
            "the trimmed key must be stored under openai"
        );

        let (provider, status) = logout("", "openai", &auth_path).expect("logout");
        assert_eq!(provider, "openai");
        assert_eq!(status, "Removed stored credentials for openai.");
        let auth = AuthStorage::load(auth_path.clone()).expect("reload auth");
        assert!(auth.get("openai").is_none());
        let (_, status) = logout("openai", "anthropic", &auth_path).expect("second logout");
        assert_eq!(status, "No stored credentials for openai.");
    }

    #[test]
    fn a_blank_api_key_fails_without_saving() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let LoginStart::Pending { pending, .. } =
            run(start_login("openai", &auth_path, &[], None)).expect("start")
        else {
            panic!("openai must start an API-key login");
        };
        match run(complete_login(pending, "   ", &auth_path)) {
            Err(LoginFailure::Failed(message)) => {
                assert!(message.contains("API key cannot be empty"), "{message}");
            }
            _ => panic!("a blank key must fail terminally"),
        }
        assert!(
            !auth_path.exists(),
            "nothing may be written for a failed login"
        );
    }

    #[test]
    fn bare_login_lists_providers_and_unknown_providers_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        match run(start_login("", &auth_path, &[], None)) {
            Ok(LoginStart::Listing(listing)) => {
                assert!(listing.contains("Available login providers:"), "{listing}");
            }
            _ => panic!("bare /login must list providers"),
        }
        match run(start_login("no-such-provider", &auth_path, &[], None)) {
            Err(message) => assert!(message.contains("Login not supported"), "{message}"),
            Ok(_) => panic!("an unknown provider must be refused"),
        }
    }
}
