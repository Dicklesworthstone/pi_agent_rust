//! Source-tree recovery utility; not an additional shipping binary.
//!
//! Usage: `recover_session_text` SOURCE [--active | --branch ID | --json]
//!        [--output `NEW_FILE`]
//!        `recover_session_text` SOURCE --session-output `NEW_SESSION.jsonl`
//!        [--branch ID | --root]
//!
//! Prefer --output over shell redirection: a shell can truncate a source file
//! before this program starts, while --output refuses every existing target.
//! No provider credentials, runtime, extensions or session initialization needed.
//!
//! Inspection is the default. --session-output explicitly creates a lossy,
//! native text-only copy with a new identity and provenance warning. All source
//! branches remain in the copy; --branch selects its initial path, while --root
//! starts a new path. The saved active path is selected otherwise. An incomplete
//! tool batch requires selecting an earlier complete entry or the root. This
//! does not execute historical tools, recover media, or repair the original.
//! Resume the NEW file using the ordinary `pi --session NEW_SESSION.jsonl`
//! entrypoint; workspace, credential and provider policies still apply.

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::path::PathBuf;

use clap::Parser;
use pi::sdk::Session;
use pi::session_recovery::{RecoveryExport, RecoverySelection};

#[derive(Parser)]
#[command(about = "Inspect session text or explicitly create a new text-only session copy")]
struct Args {
    /// Authoritative version-3 JSONL source, not a SQLite file or V2 directory.
    source: PathBuf,
    /// Create a complete new report; never replace an existing path.
    #[arg(long, conflicts_with = "session_output")]
    output: Option<PathBuf>,
    /// Explicitly create a NEW resumable text-only .jsonl copy. Never repair the source.
    #[arg(long, conflicts_with_all = ["output", "json"])]
    session_output: Option<PathBuf>,
    /// Emit the complete machine-readable report rather than readable text.
    #[arg(long, conflicts_with_all = ["active", "branch", "root", "session_output"])]
    json: bool,
    /// Show only the saved selected branch. Default: every saved record.
    #[arg(long, conflicts_with_all = ["branch", "root"])]
    active: bool,
    /// Inspect this exact path, or select it initially in an explicit session copy.
    #[arg(long, conflicts_with = "root", value_parser = entry_id)]
    branch: Option<String>,
    /// Start the session copy at the root, preserving all original history off-path.
    #[arg(long, requires = "session_output", conflicts_with_all = ["active", "branch", "json"])]
    root: bool,
}

fn entry_id(value: &str) -> Result<String, String> {
    if value.trim().is_empty() || value.len() > 1024 {
        return Err(
            "branch ID must be nonempty and at most 1024 UTF-8 bytes; use --root for a new path"
                .into(),
        );
    }
    Ok(value.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    run(&args)?;
    Ok(())
}

fn run(args: &Args) -> pi::PiResult<()> {
    if let Some(output) = &args.session_output {
        let selected = if args.root {
            Some("")
        } else {
            args.branch.as_deref()
        };
        let id = Session::fork_recovered_text(&args.source, output, selected)?;
        // The file has already been published. A closed diagnostic stream must
        // not turn that success into an error suggesting nothing was created.
        // Print only a generated UUID, not an unescaped caller-supplied path.
        let _ = writeln!(
            std::io::stderr().lock(),
            "Created text-only recovery session {id}. Resume the new file with pi --session."
        );
        return Ok(());
    }
    let report = Session::recover_text(&args.source)?;
    let selection = match args.branch.as_deref() {
        Some(id) => RecoverySelection::Branch(id),
        None if args.active => RecoverySelection::Active,
        None => RecoverySelection::All,
    };
    let format = if args.json {
        RecoveryExport::Json
    } else {
        RecoveryExport::Text(selection)
    };
    if let Some(output) = &args.output {
        report.export_file(output, format)?;
    } else {
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        match format {
            RecoveryExport::Json => report.write_json(&mut output)?,
            RecoveryExport::Text(selection) => {
                report.write_selected_text(selection, &mut output)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_remains_default_and_copy_requires_an_explicit_destination() {
        let args = Args::try_parse_from(["recover", "source.jsonl"]).unwrap();
        assert!(args.output.is_none());
        assert!(args.session_output.is_none());
        assert!(!args.root && !args.active && !args.json);
        let args = Args::try_parse_from([
            "recover",
            "source.jsonl",
            "--session-output",
            "copy.jsonl",
            "--branch",
            "chosen",
        ])
        .unwrap();
        assert_eq!(args.session_output, Some(PathBuf::from("copy.jsonl")));
        assert_eq!(args.branch.as_deref(), Some("chosen"));
    }

    #[test]
    fn incompatible_modes_and_ambiguous_empty_branch_fail_during_argument_parsing() {
        for extra in [
            vec!["--root"],
            vec!["--branch", ""],
            vec!["--active", "--branch", "id"],
            vec!["--json", "--branch", "id"],
            vec!["--session-output", "copy.jsonl", "--json"],
            vec!["--session-output", "copy.jsonl", "--output", "report.txt"],
            vec!["--session-output", "copy.jsonl", "--root", "--branch", "id"],
        ] {
            let mut argv = vec!["recover", "source.jsonl"];
            argv.extend(extra);
            assert!(Args::try_parse_from(argv).is_err());
        }
        assert!(entry_id(&"界".repeat(342)).is_err());
        assert_eq!(entry_id("exact ID ").unwrap(), "exact ID ");
    }

    #[test]
    fn explicit_copy_mode_writes_native_jsonl_while_json_mode_writes_a_report() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let mut original = serde_json::to_vec(&Session::in_memory().header).unwrap();
        original.push(b'\n');
        std::fs::write(&source, &original).unwrap();
        let copy = directory.path().join("copy.jsonl");
        let args = Args::try_parse_from([
            "recover",
            source.to_str().unwrap(),
            "--session-output",
            copy.to_str().unwrap(),
            "--root",
        ])
        .unwrap();
        run(&args).unwrap();
        let copied = std::fs::read_to_string(&copy).unwrap();
        let rows: Vec<serde_json::Value> = copied
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["type"], "session");
        assert_eq!(rows[0]["leafId"], rows[1]["id"]);
        assert!(rows[1]["parentId"].is_null());
        assert!(run(&args).is_err(), "copy mode is create-only too");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), copied);

        let output = directory.path().join("report.json");
        let args = Args::try_parse_from([
            "recover",
            source.to_str().unwrap(),
            "--json",
            "--output",
            output.to_str().unwrap(),
        ])
        .unwrap();
        run(&args).unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
        assert_eq!(report["schema"], "pi.session_text_recovery.v1");
        assert_eq!(std::fs::read(source).unwrap(), original);
    }
}
