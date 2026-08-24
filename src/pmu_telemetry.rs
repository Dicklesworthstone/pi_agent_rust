#![forbid(unsafe_code)]

//! PMU-guided stall-cycle elimination and microarchitectural regression budgets.
//!
//! Provides hardware-level performance monitoring counter (PMU) sample models,
//! derived microarchitectural ratios (IPC, LLC miss rate, branch miss rate, stall ratios),
//! regression budget evaluation, and opportunity ranking for extension fast paths.

use serde::{Deserialize, Serialize};

/// Schema identifier for versioned PMU sample logs.
pub const PMU_TELEMETRY_SCHEMA: &str = "pi.pmu.telemetry.v1";

/// Hardware PMU counter measurements for an execution interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PmuSample {
    /// Total elapsed CPU cycles.
    pub cycles: u64,
    /// Total retired instructions.
    pub instructions: u64,
    /// Last-level cache (LLC) reference requests.
    pub llc_references: u64,
    /// Last-level cache (LLC) misses.
    pub llc_misses: u64,
    /// Branch instructions executed.
    pub branch_instructions: u64,
    /// Mispredicted branches.
    pub branch_misses: u64,
    /// Cycles where the execution pipeline stalled waiting for the frontend.
    pub frontend_stall_cycles: u64,
    /// Cycles where the execution pipeline stalled waiting for backend/memory.
    pub backend_stall_cycles: u64,
}

impl Default for PmuSample {
    fn default() -> Self {
        Self {
            cycles: 0,
            instructions: 0,
            llc_references: 0,
            llc_misses: 0,
            branch_instructions: 0,
            branch_misses: 0,
            frontend_stall_cycles: 0,
            backend_stall_cycles: 0,
        }
    }
}

impl PmuSample {
    /// Instructions retired per clock cycle (IPC). Higher is better.
    #[must_use]
    pub fn ipc(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            self.instructions as f64 / self.cycles as f64
        }
    }

    /// Last-level cache (LLC) miss rate (0.0 to 1.0).
    #[must_use]
    pub fn llc_miss_rate(&self) -> f64 {
        if self.llc_references == 0 {
            0.0
        } else {
            (self.llc_misses as f64 / self.llc_references as f64).clamp(0.0, 1.0)
        }
    }

    /// Branch misprediction rate (0.0 to 1.0).
    #[must_use]
    pub fn branch_miss_rate(&self) -> f64 {
        if self.branch_instructions == 0 {
            0.0
        } else {
            (self.branch_misses as f64 / self.branch_instructions as f64).clamp(0.0, 1.0)
        }
    }

    /// Fraction of cycles stalled on the frontend pipeline (0.0 to 1.0).
    #[must_use]
    pub fn frontend_stall_ratio(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            (self.frontend_stall_cycles as f64 / self.cycles as f64).clamp(0.0, 1.0)
        }
    }

    /// Fraction of cycles stalled on the backend / memory subsystem (0.0 to 1.0).
    #[must_use]
    pub fn backend_stall_ratio(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            (self.backend_stall_cycles as f64 / self.cycles as f64).clamp(0.0, 1.0)
        }
    }

    /// Total stall ratio combining frontend and backend (0.0 to 1.0).
    #[must_use]
    pub fn total_stall_ratio(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            let total_stalls = self
                .frontend_stall_cycles
                .saturating_add(self.backend_stall_cycles);
            (total_stalls as f64 / self.cycles as f64).clamp(0.0, 1.0)
        }
    }
}

/// Thresholds defining microarchitectural regression limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PmuRegressionBudget {
    /// Maximum allowed LLC miss rate (e.g. 0.25 = 25%).
    pub max_llc_miss_rate: f64,
    /// Maximum allowed branch misprediction rate (e.g. 0.05 = 5%).
    pub max_branch_miss_rate: f64,
    /// Maximum allowed total pipeline stall ratio (e.g. 0.40 = 40%).
    pub max_stall_ratio: f64,
    /// Minimum required IPC (e.g. 1.0 instructions/cycle).
    pub min_ipc: f64,
}

impl Default for PmuRegressionBudget {
    fn default() -> Self {
        Self {
            max_llc_miss_rate: 0.25,
            max_branch_miss_rate: 0.05,
            max_stall_ratio: 0.40,
            min_ipc: 1.0,
        }
    }
}

/// Evaluation outcome from checking a [`PmuSample`] against a [`PmuRegressionBudget`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PmuBudgetVerdict {
    /// True if all microarchitectural thresholds were satisfied.
    pub passed: bool,
    /// List of exceeded threshold descriptions.
    pub violations: Vec<String>,
    /// Summary line.
    pub summary: String,
}

impl PmuRegressionBudget {
    /// Evaluate a [`PmuSample`] against this budget's thresholds.
    #[must_use]
    pub fn evaluate(&self, sample: &PmuSample) -> PmuBudgetVerdict {
        let mut violations = Vec::new();

        let llc_rate = sample.llc_miss_rate();
        if llc_rate > self.max_llc_miss_rate {
            violations.push(format!(
                "LLC miss rate {:.2}% exceeds budget max {:.2}%",
                llc_rate * 100.0,
                self.max_llc_miss_rate * 100.0
            ));
        }

        let branch_rate = sample.branch_miss_rate();
        if branch_rate > self.max_branch_miss_rate {
            violations.push(format!(
                "Branch miss rate {:.2}% exceeds budget max {:.2}%",
                branch_rate * 100.0,
                self.max_branch_miss_rate * 100.0
            ));
        }

        let stall_ratio = sample.total_stall_ratio();
        if stall_ratio > self.max_stall_ratio {
            violations.push(format!(
                "Total stall ratio {:.2}% exceeds budget max {:.2}%",
                stall_ratio * 100.0,
                self.max_stall_ratio * 100.0
            ));
        }

        let ipc = sample.ipc();
        if ipc < self.min_ipc && sample.cycles > 1000 {
            violations.push(format!(
                "IPC {:.2} falls below budget minimum {:.2}",
                ipc, self.min_ipc
            ));
        }

        let passed = violations.is_empty();
        let summary = if passed {
            format!(
                "PMU budget PASSED: IPC={:.2}, LLC miss={:.1}%, Stalls={:.1}%",
                ipc,
                llc_rate * 100.0,
                stall_ratio * 100.0
            )
        } else {
            format!("PMU budget FAILED with {} violation(s)", violations.len())
        };

        PmuBudgetVerdict {
            passed,
            violations,
            summary,
        }
    }
}

/// Scored optimization opportunity identified by PMU telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PmuOptimizationOpportunity {
    /// Identifier or component name (e.g. "extension_dispatch", "json_canonicalize").
    pub name: String,
    /// Estimated recoverable stall cycles.
    pub recoverable_stall_cycles: u64,
    /// Estimated speedup potential (e.g. 1.35 = 35% speedup).
    pub estimated_speedup: f64,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f64,
    /// Diagnostic bottleneck category (e.g. "memory_bound_llc", "branch_heavy").
    pub bottleneck_category: String,
}

/// Opportunity ranker based on hardware stall telemetry.
#[derive(Debug, Default, Clone)]
pub struct PmuOpportunityRanker;

impl PmuOpportunityRanker {
    /// Analyze a named component's [`PmuSample`] and evaluate optimization potential.
    #[must_use]
    pub fn score_candidate(name: &str, sample: &PmuSample) -> PmuOptimizationOpportunity {
        let backend_stalls = sample.backend_stall_cycles;
        let frontend_stalls = sample.frontend_stall_cycles;
        let total_stalls = backend_stalls.saturating_add(frontend_stalls);

        let bottleneck_category = if sample.llc_miss_rate() > 0.30 {
            "memory_bound_llc".to_string()
        } else if sample.branch_miss_rate() > 0.08 {
            "branch_mispredict_heavy".to_string()
        } else if sample.frontend_stall_ratio() > 0.25 {
            "frontend_instruction_starvation".to_string()
        } else {
            "compute_bound".to_string()
        };

        // Estimated recoverable cycles is 60% of stalls for memory/branch bottlenecks
        let recoverable_stall_cycles = total_stalls.saturating_mul(6) / 10;
        let speedup = if sample.cycles == 0 {
            1.0
        } else {
            let active_cycles = sample.cycles.saturating_sub(recoverable_stall_cycles);
            if active_cycles == 0 {
                1.0
            } else {
                (sample.cycles as f64 / active_cycles as f64).clamp(1.0, 5.0)
            }
        };

        let confidence = if sample.cycles > 50_000 {
            0.95
        } else if sample.cycles > 5_000 {
            0.80
        } else {
            0.50
        };

        PmuOptimizationOpportunity {
            name: name.to_string(),
            recoverable_stall_cycles,
            estimated_speedup: speedup,
            confidence,
            bottleneck_category,
        }
    }
}
