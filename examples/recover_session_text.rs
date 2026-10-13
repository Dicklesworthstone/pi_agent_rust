//! Read-only source-tree recovery utility; not an additional shipping binary.
//!
//! Usage: recover_session_text SOURCE [--active | --branch ID | --json]
//!        [--output NEW_FILE]
//!
//! Prefer --output over shell redirection: a shell can truncate a source file
//! before this program starts, while --output refuses every existing target.
//! No provider credentials, runtime, extensions or session initialization needed.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::Parser;
use pi::sdk::Session;
use pi::session_recovery::{RecoveryExport, RecoverySelection};

#[derive(Parser)]
#[command(about = "Inspect session text without reading media sidecars or modifying the source")]
struct Args {
    /// Authoritative version-3 JSONL source, not a SQLite file or V2 directory.
    source: PathBuf,
    /// Create a complete new report; never replace an existing path.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Emit the complete machine-readable report rather than readable text.
    #[arg(long, conflicts_with_all = ["active", "branch"])]
    json: bool,
    /// Show only the saved selected branch. Default: every saved record.
    #[arg(long, conflicts_with = "branch")]
    active: bool,
    /// Show the root-to-entry path ending at this exact saved ID.
    #[arg(long)]
    branch: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
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
            RecoveryExport::Text(selection) => report.write_selected_text(selection, &mut output)?,
        }
    }
    Ok(())
}
