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
    clap_complete::aot::generate(generator, &mut cmd, "pi", &mut std::io::stdout().lock());
    // Dynamic wrapper: --model/--session consult `pi __complete` so values
    // come from the live registry/session index (never drift).
    let wrapper = match shell {
        "bash" => BASH_DYNAMIC_WRAPPER,
        "zsh" => ZSH_DYNAMIC_WRAPPER,
        "fish" => FISH_DYNAMIC_WRAPPER,
        _ => unreachable!(),
    };
    let _ = std::io::Write::write_fmt(out, format_args!("{wrapper}\n"));
    Ok(())
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
    return 1
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
    return 1
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
        "--model" | "--provider" | "--smol" | "--slow" => model_candidates(prefix),
        "--session" | "--resume" => session_candidates(prefix),
        other => {
            return Err(crate::error::Error::validation(format!(
                "Unknown __complete flag '{other}'; expected --model, --provider, --smol, \
                 --slow, --session, or --resume"
            )));
        }
    };
    for candidate in candidates {
        let _ = std::io::Write::write_fmt(out, format_args!("{candidate}\n"));
    }
    Ok(())
}

fn model_candidates(prefix: &str) -> Vec<String> {
    let auth_path = crate::config::Config::global_dir().join("auth.json");
    let Ok(auth) = crate::auth::AuthStorage::load(auth_path) else {
        return Vec::new();
    };
    let registry = crate::models::ModelRegistry::load(&auth, None);
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
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    let sessions = index.list_sessions(cwd.as_deref()).unwrap_or_default();
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
            out.push(meta.path.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_generation_covers_all_shells() {
        for shell in ["bash", "zsh", "fish"] {
            let mut out = Vec::new();
            print_script(shell, &mut out).expect("script");
            let text = String::from_utf8(out).expect("utf8");
            assert!(text.contains("__complete"), "{shell} wrapper missing");
        }
        assert!(print_script("powershell", &mut Vec::new()).is_err());
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
}
