#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use pi::extension_scoring::{CandidateInput, score_candidates};
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(name = "ext_score_candidates")]
#[command(about = "Score extension candidates and emit a ranked list")]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    summary_out: Option<PathBuf>,
    #[arg(long)]
    as_of: Option<String>,
    #[arg(long)]
    generated_at: Option<String>,
    #[arg(long, default_value_t = 10)]
    top_n: usize,
    #[arg(long, default_value_t = false)]
    check: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InputFile {
    List(Vec<CandidateInput>),
    Document(InputDocument),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputDocument {
    generated_at: Option<String>,
    candidates: Vec<CandidateInput>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let bytes = fs::read(&args.input).with_context(|| format!("read {}", args.input.display()))?;
    let input: InputFile =
        serde_json::from_slice(&bytes).context("parse candidate scoring input")?;

    let (candidates, embedded_generated_at) = match input {
        InputFile::List(list) => (list, None),
        InputFile::Document(doc) => (doc.candidates, doc.generated_at),
    };

    let as_of = parse_timestamp(args.as_of, "as_of")?.unwrap_or_else(Utc::now);
    let generated_at = parse_timestamp(args.generated_at, "generated_at")?
        .or_else(|| {
            parse_timestamp(embedded_generated_at, "generated_at")
                .ok()
                .flatten()
        })
        .unwrap_or(as_of);

    let report = score_candidates(&candidates, as_of, generated_at, args.top_n);
    let json = serde_json::to_string_pretty(&report).context("serialize report")?;
    let json = format!("{json}\n");

    if args.check {
        match fs::read_to_string(&args.out) {
            Ok(existing) => {
                if existing != json {
                    bail!("Generated report differs from {}", args.out.display());
                }
            }
            Err(_) => bail!("Missing output file: {}", args.out.display()),
        }
    } else {
        fs::write(&args.out, json).with_context(|| format!("write {}", args.out.display()))?;
    }

    if let Some(summary_path) = args.summary_out {
        let summary_json =
            serde_json::to_string_pretty(&report.summary).context("serialize summary")?;
        fs::write(&summary_path, format!("{summary_json}\n"))
            .with_context(|| format!("write {}", summary_path.display()))?;
    }

    Ok(())
}

fn parse_timestamp(value: Option<String>, label: &str) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed =
        DateTime::parse_from_rfc3339(&value).with_context(|| format!("parse {label} timestamp"))?;
    Ok(Some(parsed.with_timezone(&Utc)))
}
