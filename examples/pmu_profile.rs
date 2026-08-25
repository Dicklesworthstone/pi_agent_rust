#![forbid(unsafe_code)]

//! Hardware PMU performance profiling and regression budget verification CLI.
//!
//! Usage:
//!   cargo run --example `pmu_profile` -- eval --cycles 100000 --instructions 150000 --llc-refs 2000 --llc-misses 100

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use pi::pmu_telemetry::{
    PMU_TELEMETRY_SCHEMA, PmuOpportunityRanker, PmuRegressionBudget, PmuSample,
};

#[derive(Debug, Parser)]
#[command(name = "pmu_profile")]
#[command(about = "PMU performance profiling and regression budget evaluation")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    /// Evaluate a PMU sample against the microarchitectural regression budget.
    Eval(EvalArgs),
    /// Score an optimization opportunity from measured PMU counters.
    Score(ScoreArgs),
}

#[derive(Debug, Args)]
struct EvalArgs {
    #[arg(long, default_value_t = 100_000)]
    cycles: u64,
    #[arg(long, default_value_t = 150_000)]
    instructions: u64,
    #[arg(long, default_value_t = 2_000)]
    llc_refs: u64,
    #[arg(long, default_value_t = 100)]
    llc_misses: u64,
    #[arg(long, default_value_t = 20_000)]
    branches: u64,
    #[arg(long, default_value_t = 200)]
    branch_misses: u64,
    #[arg(long, default_value_t = 5_000)]
    frontend_stalls: u64,
    #[arg(long, default_value_t = 10_000)]
    backend_stalls: u64,
}

#[derive(Debug, Args)]
struct ScoreArgs {
    #[arg(long, default_value = "hostcall_pipeline")]
    name: String,
    #[arg(long, default_value_t = 200_000)]
    cycles: u64,
    #[arg(long, default_value_t = 100_000)]
    instructions: u64,
    #[arg(long, default_value_t = 5_000)]
    llc_refs: u64,
    #[arg(long, default_value_t = 1_500)]
    llc_misses: u64,
    #[arg(long, default_value_t = 10_000)]
    frontend_stalls: u64,
    #[arg(long, default_value_t = 60_000)]
    backend_stalls: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CommandMode::Eval(args) => {
            let sample = PmuSample {
                cycles: args.cycles,
                instructions: args.instructions,
                llc_references: args.llc_refs,
                llc_misses: args.llc_misses,
                branch_instructions: args.branches,
                branch_misses: args.branch_misses,
                frontend_stall_cycles: args.frontend_stalls,
                backend_stall_cycles: args.backend_stalls,
            };

            let budget = PmuRegressionBudget::default();
            let verdict = budget.evaluate(&sample);

            println!("Schema: {PMU_TELEMETRY_SCHEMA}");
            println!("IPC: {:.2}", sample.ipc());
            println!("LLC Miss Rate: {:.2}%", sample.llc_miss_rate() * 100.0);
            println!(
                "Branch Miss Rate: {:.2}%",
                sample.branch_miss_rate() * 100.0
            );
            println!(
                "Total Stall Ratio: {:.2}%",
                sample.total_stall_ratio() * 100.0
            );
            println!("Verdict: {}", verdict.summary);

            if !verdict.passed {
                for v in &verdict.violations {
                    eprintln!("  - Violation: {v}");
                }
                bail!("PMU regression budget check failed");
            }
        }
        CommandMode::Score(args) => {
            let sample = PmuSample {
                cycles: args.cycles,
                instructions: args.instructions,
                llc_references: args.llc_refs,
                llc_misses: args.llc_misses,
                branch_instructions: 10_000,
                branch_misses: 200,
                frontend_stall_cycles: args.frontend_stalls,
                backend_stall_cycles: args.backend_stalls,
            };

            let opp = PmuOpportunityRanker::score_candidate(&args.name, &sample);
            println!("Component: {}", opp.name);
            println!("Bottleneck Category: {}", opp.bottleneck_category);
            println!("Estimated Speedup: {:.2}x", opp.estimated_speedup);
            println!("Confidence: {:.0}%", opp.confidence * 100.0);
            println!("Recoverable Stall Cycles: {}", opp.recoverable_stall_cycles);
        }
    }
    Ok(())
}
