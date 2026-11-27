//! Shell completions (bd-cv653.7.2).
//!
//! `pi completions <bash|zsh|fish>` prints the clap_complete script from
//! the live derive graph (always in sync by construction) plus a dynamic
//! wrapper wiring `--model`/`--session` to the `__complete` protocol.
//! `pi __complete <flag> [prefix]` answers dynamic candidates from the live
//! model registry and the session index, so completions never drift from
//! the actual CLI.

use clap::CommandFactory;

use crate::error::Result;

/// Print the completion script for a shell.
///
/// # Errors
/// Named validation error for unknown shells.
pub fn print_script(shell: &str, out: &mut dyn std::io::Write) -> Result<()> {
    let generator = match shell {
        "bash" => clap_complete::aot::Shell::Bash,
        "zsh" => clap_complete::aot::Shell::Zsh,
        "fish" => clap_complete::aot::Shell::Fish,
        other => {
            return Err(crate::error::Error::validation(format!(
                "Unknown shell '{other}'; expected bash, zsh, or fish"
            )));
        }
    };
    let mut cmd = crate::cli::Cli::command();
    // Generate into an in-memory buffer first: `clap_complete::aot::generate`
    // panics internally on any `io::Error` (including `BrokenPipe` when piped
    // to `head`/`less`), and previously bypassed `out` by locking `stdout`
    // directly.
    let mut script = Vec::new();
    clap_complete::aot::generate(generator, &mut cmd, "pi", &mut script);
    // Dynamic wrapper: --model/--session consult `pi __complete` so values
    // come from the live registry/session index (never drift).
    let wrapper = match shell {
        "bash" => BASH_DYNAMIC_WRAPPER,
        "zsh" => ZSH_DYNAMIC_WRAPPER,
        "fish" => FISH_DYNAMIC_WRAPPER,
        _ => unreachable!(),
    };
    if write_ignoring_broken_pipe(out, &script)?
        && write_ignoring_broken_pipe(out, wrapper.as_bytes())?
    {
        let _ = write_ignoring_broken_pipe(out, b"\n")?;
    }
    Ok(())
}

fn write_ignoring_broken_pipe(out: &mut dyn std::io::Write, bytes: &[u8]) -> Result<bool> {
    match out.write_all(bytes) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err.into()),
    }
}

#[allow(clippy::needless_raw_string_hashes)]
const BASH_DYNAMIC_WRAPPER: &str = r#"
# pi dynamic values (bd-cv653.7.2): models + sessions via `pi __complete`
_pi_dynamic() {
    local flag="${COMP_WORDS[COMP_CWORD-1]}"
    case "$flag" in
        --model|--provider|--smol|--slow|--session|--resume)
            COMPREPLY=($(compgen -W "$(pi __complete "$flag" "${COMP_WORDS[COMP_CWORD]}" 2>/dev/null)" -- "${COMP_WORDS[COMP_CWORD]}"))
            return 0
            ;;
    esac
    # Preserve clap's full subcommand/flag/file completion for every other word.
    # This wrapper only takes ownership of the six dynamic value positions.
    _pi "$@"
}
complete -o default -F _pi_dynamic pi 2>/dev/null || true
"#;

#[allow(clippy::needless_raw_string_hashes)]
const ZSH_DYNAMIC_WRAPPER: &str = r#"
# pi dynamic values (bd-cv653.7.2): models + sessions via `pi __complete`
_pi_dynamic() {
    local flag="${words[CURRENT-1]}"
    case "$flag" in
        --model|--provider|--smol|--slow|--session|--resume)
            local -a candidates
            candidates=(${(f)"$(pi __complete "$flag" "${words[CURRENT]}" 2>/dev/null)"})
            _describe 'values' candidates
            return 0
            ;;
    esac
    # Hand ordinary positions back to the generated completion function.
    _pi "$@"
}
compdef _pi_dynamic pi 2>/dev/null || true
"#;

#[allow(clippy::needless_raw_string_hashes)]
const FISH_DYNAMIC_WRAPPER: &str = r#"
# pi dynamic values (bd-cv653.7.2): models + sessions via `pi __complete`
function __pi_dynamic
    set -l tokens (commandline -opc)
    set -l flag $tokens[-1]
    switch $flag
        case --model --provider --smol --slow --session --resume
            pi __complete $flag (commandline -ct) 2>/dev/null
    end
end
complete -c pi -f -a '(__pi_dynamic)' 2>/dev/null || true
"#;

/// Answer `pi __complete <flag> [prefix]` candidates (one per line).
///
/// # Errors
/// Named validation error for unknown flags.
pub fn complete(flag: &str, prefix: &str, out: &mut dyn std::io::Write) -> Result<()> {
    let candidates = match flag {
        "--model" | "--smol" | "--slow" => model_candidates(prefix),
        "--provider" => provider_candidates(prefix),
        "--session" | "--resume" => session_candidates(prefix),
        other => {
            return Err(crate::error::Error::validation(format!(
                "Unknown __complete flag '{other}'; expected --model, --provider, --smol, \
                 --slow, --session, or --resume"
            )));
        }
    };
    write_candidates(candidates, out)
}

fn write_candidates(candidates: Vec<String>, out: &mut dyn std::io::Write) -> Result<()> {
    for mut candidate in candidates {
        candidate.push('\n');
        if !write_ignoring_broken_pipe(out, candidate.as_bytes())? {
            break;
        }
    }
    Ok(())
}

fn collect_provider_candidates<'a>(
    providers: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> Vec<String> {
    let lowered = prefix.to_ascii_lowercase();
    let mut unique = std::collections::BTreeSet::new();
    for provider in providers {
        if lowered.is_empty() || provider.to_ascii_lowercase().contains(&lowered) {
            unique.insert(provider);
        }
    }
    unique.into_iter().map(str::to_owned).collect()
}

fn provider_candidates(prefix: &str) -> Vec<String> {
    let registry =
        crate::models::ModelRegistry::load_for_listing_with_credential_resolver(None, |_| None);
    collect_provider_candidates(
        registry
            .models()
            .iter()
            .map(|entry| entry.model.provider.as_str()),
        prefix,
    )
}

fn model_candidates(prefix: &str) -> Vec<String> {
    let registry =
        crate::models::ModelRegistry::load_for_listing_with_credential_resolver(None, |_| None);
    let lowered = prefix.to_ascii_lowercase();
    let mut out = Vec::new();
    for entry in registry.models() {
        let qualified = format!("{}/{}", entry.model.provider, entry.model.id);
        if lowered.is_empty() || qualified.to_ascii_lowercase().contains(&lowered) {
            out.push(qualified);
        }
        if entry.model.id.to_ascii_lowercase().contains(&lowered) {
            out.push(entry.model.id.clone()); // ubs:ignore owned String required by the candidates vec
        }
    }
    out.sort();
    out.dedup();
    out
}

fn session_candidates(prefix: &str) -> Vec<String> {
    let index = crate::session_index::SessionIndex::new();
    let cwd = std::env::current_dir().ok();
    session_candidates_in(&index, cwd.as_deref(), prefix)
}

fn session_candidates_in(
    index: &crate::session_index::SessionIndex,
    cwd: Option<&std::path::Path>,
    prefix: &str,
) -> Vec<String> {
    let sessions = match cwd {
        Some(dir) => index.list_sessions_for_workspace(dir),
        None => index.list_sessions(None),
    }
    .unwrap_or_default();
    let lowered = prefix.to_ascii_lowercase();
    let mut out = Vec::new();
    for meta in sessions {
        if lowered.is_empty()
            || meta.id.to_ascii_lowercase().contains(&lowered)
            || meta.path.to_ascii_lowercase().contains(&lowered)
            || meta
                .name
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains(&lowered)
        {
            out.push(meta.path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_generation_covers_all_shells() {
        for (shell, expected_clap_marker) in [
            ("bash", "_pi()"),
            ("zsh", "#compdef pi"),
            ("fish", "complete -c pi"),
        ] {
            let mut out = Vec::new();
            print_script(shell, &mut out).expect("script");
            let text = String::from_utf8(out).expect("utf8");
            // ubs:ignore[rust.panic.assert-macros] -- Test-only completion regression oracle.
            assert!(
                text.contains(expected_clap_marker),
                "{shell} clap_complete output missing from writer ({expected_clap_marker})"
            );
            assert!(text.contains("__complete"), "{shell} wrapper missing");
        }
        assert!(print_script("powershell", &mut Vec::new()).is_err());
    }

    #[test]
    fn dynamic_shell_wrappers_preserve_generated_completions() {
        for (shell, delegate, wrapper_registration) in [
            ("bash", "    _pi \"$@\"", "complete -o default -F _pi_dynamic pi"),
            ("zsh", "    _pi \"$@\"", "compdef _pi_dynamic pi"),
        ] {
            let mut output = Vec::new();
            print_script(shell, &mut output).expect("completion script");
            let script = String::from_utf8(output).expect("UTF-8 completion script");
            let wrapper = script
                .rfind("_pi_dynamic() {")
                .map(|offset| &script[offset..])
                .expect("dynamic wrapper is emitted");
            assert!(
                wrapper.contains(delegate),
                "{shell} must delegate non-dynamic words to clap's handler"
            );
            assert!(
                wrapper.contains(wrapper_registration),
                "{shell} must register its delegating wrapper"
            );
            assert!(
                wrapper.find(delegate) < wrapper.find(wrapper_registration),
                "{shell} should delegate before registering the wrapper"
            );
        }
    }

    #[test]
    fn print_script_ignores_broken_pipe_without_panicking() -> Result<()> {
        struct CountingBrokenPipeWriter {
            writes: usize,
        }
        impl std::io::Write for CountingBrokenPipeWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                self.writes += 1;
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "broken pipe",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for shell in ["bash", "zsh", "fish"] {
            let mut writer = CountingBrokenPipeWriter { writes: 0 };
            print_script(shell, &mut writer)?;
            assert_eq!(writer.writes, 1, "{shell} should stop on first BrokenPipe");
        }
        let mut writer = CountingBrokenPipeWriter { writes: 0 };
        write_candidates(
            vec!["anthropic".to_string(), "openai".to_string()],
            &mut writer,
        )?;
        assert_eq!(writer.writes, 1, "complete should stop on first BrokenPipe");
        let mut model_writer = CountingBrokenPipeWriter { writes: 0 };
        complete("--model", "", &mut model_writer)?;
        let mut provider_writer = CountingBrokenPipeWriter { writes: 0 };
        complete("--provider", "", &mut provider_writer)?;
        Ok(())
    }

    #[test]
    fn print_script_propagates_non_broken_pipe_io_errors() {
        struct PermissionDeniedWriter;
        impl std::io::Write for PermissionDeniedWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "write denied",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // ubs:ignore[rust.panic.assert-macros] -- Test-only completion regression oracle.
        assert!(print_script("bash", &mut PermissionDeniedWriter).is_err());
        // ubs:ignore[rust.panic.assert-macros] -- Test-only completion regression oracle.
        assert!(
            write_candidates(vec!["anthropic".to_string()], &mut PermissionDeniedWriter).is_err()
        );
    }

    #[test]
    fn provider_candidates_returns_unique_provider_names_only() {
        assert_eq!(
            collect_provider_candidates(["openai", "anthropic", "openai", "google"], ""),
            vec![
                "anthropic".to_string(),
                "google".to_string(),
                "openai".to_string(),
            ]
        );
        assert_eq!(
            collect_provider_candidates(["openai", "anthropic", "openai", "google"], "ANTH"),
            vec!["anthropic".to_string()]
        );

        let providers = provider_candidates("");
        // ubs:ignore[rust.panic.assert-macros] -- Test-only completion regression oracle.
        assert!(
            !providers.is_empty(),
            "built-in catalog should always provide completion providers"
        );
        // ubs:ignore[rust.panic.assert-macros] -- Test-only completion regression oracle.
        assert!(
            providers.iter().any(|name| name == "anthropic"),
            "built-in provider list should include anthropic: {providers:?}"
        );
        // ubs:ignore[rust.panic.assert-macros] -- Test-only completion regression oracle.
        assert!(
            providers.iter().all(|name| !name.contains('/')),
            "provider completions must not include provider/model paths: {providers:?}"
        );
        let mut sorted = providers.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(providers, sorted);
    }

    #[test]
    fn unknown_complete_flag_is_named_error() {
        let err = complete("--bogus", "", &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("Unknown __complete flag"));
    }

    #[test]
    fn model_candidates_filter_case_insensitively() {
        // The registry may be empty in test envs; the filter must still
        // behave (no panic, prefix containment).
        let all = model_candidates("");
        let filtered = model_candidates("ANTHROPIC");
        assert!(filtered.len() <= all.len());
    }

    #[test]
    fn session_candidates_match_canonical_workspace_spellings() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        let other = tmp.path().join("other");
        let storage = tmp.path().join("sessions");
        std::fs::create_dir(&project).expect("project dir");
        std::fs::create_dir(&other).expect("other dir");
        std::fs::create_dir(&storage).expect("storage dir");

        let canonical_project = project.canonicalize().expect("canonical project");
        let canonical_other = other.canonicalize().expect("canonical other");
        let index = crate::session_index::SessionIndex::for_sessions_root(&storage);
        index
            .upsert_session_meta(crate::session_index::SessionMeta {
                path: storage.join("target.jsonl").display().to_string(),
                id: "sess-target".to_string(),
                cwd: canonical_project.display().to_string(),
                timestamp: "2026-11-27T00:00:00Z".to_string(),
                message_count: 2,
                last_modified_ms: 2_000,
                size_bytes: 128,
                name: Some("Alpha Work".to_string()),
            })
            .expect("upsert target");
        index
            .upsert_session_meta(crate::session_index::SessionMeta {
                path: storage.join("other.jsonl").display().to_string(),
                id: "sess-other".to_string(),
                cwd: canonical_other.display().to_string(),
                timestamp: "2026-11-27T00:00:00Z".to_string(),
                message_count: 1,
                last_modified_ms: 1_000,
                size_bytes: 64,
                name: Some("Other".to_string()),
            })
            .expect("upsert other");

        let dot_spelling = project.join(".");
        let matched = session_candidates_in(&index, Some(&dot_spelling), "alpha");
        assert_eq!(
            matched,
            vec![storage.join("target.jsonl").display().to_string()]
        );
        assert!(
            session_candidates_in(&index, Some(&dot_spelling), "other").is_empty(),
            "unrelated workspace sessions must not leak into completion candidates"
        );
    }
}
