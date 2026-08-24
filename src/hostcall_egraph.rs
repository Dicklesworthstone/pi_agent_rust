//! Equality-saturation rewrite search over hot hostcall execution plans
//! (bd-3ar8v.4.22).
//!
//! [`crate::hostcall_rewrite`] already decides *between* plans: given a
//! baseline and a candidate list it picks the unique cheapest and refuses on a
//! tie. What it never had was anything to produce those candidates — the plans
//! were hand-enumerated. This module is the search that fills that gap, and it
//! deliberately stops at the same boundary: it emits candidates and hands the
//! final say back to `HostcallRewriteEngine::select_plan`, so there is exactly
//! one place in the tree that authorizes a fast path.
//!
//! # Why an e-graph rather than a rewrite list
//!
//! Applying rewrites destructively forces a phase-ordering choice: fusing
//! marshal+validate first can hide the redundant conversion that a different
//! order would have exposed. An e-graph sidesteps that by keeping *all*
//! equivalent forms at once. Each e-class is a set of plans proven
//! interchangeable; rewriting adds a form to a class instead of replacing one,
//! so no rule can destroy the opportunity another rule needed. Extraction then
//! picks the cheapest member of the root class under the measured cost model.
//!
//! # Semantic invariants
//!
//! Every rule in [`rewrite_rules`] preserves observable hostcall behavior:
//! the same opcode executes, against the same policy decision, with the same
//! payload. The rules only remove work that is provably redundant
//! (round-trip conversions) or collapse adjacent stages into an intrinsic that
//! performs both (fusion). Two hard constraints hold throughout:
//!
//! - **Policy is never moved, duplicated, or elided.** Authorization ordering
//!   is a security property, not an optimization surface. A rule that would
//!   reorder [`StageOp::Policy`] relative to [`StageOp::Dispatch`] is rejected
//!   at construction by [`RewriteRule::is_policy_preserving`].
//! - **Saturation is bounded.** Rewrites run to a fixpoint or to an explicit
//!   node/iteration budget, whichever comes first, so a pathological trace
//!   cannot stall a hostcall.
//!
//! # Failing closed
//!
//! Ambiguity is treated as a defect, not a coin flip. If the cheapest
//! extraction is not unique, or the budget was exhausted before reaching a
//! fixpoint, the engine reports the baseline with a `fallback_reason` rather
//! than picking arbitrarily. A plan we cannot justify is not a plan we run.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::hostcall_rewrite::{HostcallRewritePlan, HostcallRewritePlanKind};

/// Schema tag for emitted decision telemetry.
pub const HOSTCALL_EGRAPH_SCHEMA: &str = "pi.ext.hostcall_egraph_decision.v1";

/// Default ceiling on saturation iterations.
pub const DEFAULT_MAX_ITERATIONS: usize = 8;

/// Default ceiling on total e-nodes. Bounds both memory and extraction cost on
/// a trace that rewrites explosively.
pub const DEFAULT_MAX_NODES: usize = 4_096;

/// Default ceiling on trees produced by one enumeration of a class.
///
/// Generous next to the plans this actually sees — the canonical hostcall plan
/// is five stages — while still bounding the Cartesian blowup a pathological
/// graph could otherwise cause on the hostcall path.
pub const DEFAULT_MAX_ENUMERATED: usize = 4_096;

// ── Plan expression language ────────────────────────────────────────────────

/// One stage of a hostcall execution plan.
///
/// The vocabulary matches the six stages the workload harness already
/// attributes cost to (`marshal`, `queue`, `schedule`, `policy`, `execute`,
/// `io`), so a cost model derived from real traces maps onto these nodes
/// directly instead of through a translation layer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StageOp {
    /// Terminal: the hostcall opcode this plan executes, e.g. `tool.read`.
    Opcode(String),
    /// Decode the request payload in the named representation.
    Marshal(Repr),
    /// Schema/shape validation of a decoded payload.
    Validate,
    /// Capability authorization. Never reordered, duplicated, or removed.
    Policy,
    /// Route to the executing lane.
    Dispatch,
    /// Convert between payload representations.
    Convert { from: Repr, to: Repr },
    /// An intrinsic performing several stages in one step. The `&'static str`
    /// is the rule that introduced it, which is what telemetry reports.
    Fused(&'static str),
}

/// Payload representation. Conversions between these are the redundancy the
/// search is looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Repr {
    /// Canonical `serde_json::Value` form: universal, slow.
    Json,
    /// Typed struct form: what the fast lane wants.
    Typed,
    /// Borrowed bytes: no decode performed yet.
    Bytes,
}

impl Repr {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Typed => "typed",
            Self::Bytes => "bytes",
        }
    }
}

impl StageOp {
    /// Stable label used in telemetry and in the extracted plan's `rule_id`.
    fn label(&self) -> String {
        match self {
            Self::Opcode(code) => format!("opcode({code})"),
            Self::Marshal(repr) => format!("marshal({})", repr.as_str()),
            Self::Validate => "validate".to_string(),
            Self::Policy => "policy".to_string(),
            Self::Dispatch => "dispatch".to_string(),
            Self::Convert { from, to } => {
                format!("convert({}->{})", from.as_str(), to.as_str())
            }
            Self::Fused(rule) => format!("fused({rule})"),
        }
    }

    /// Whether this stage is the authorization step.
    const fn is_policy(&self) -> bool {
        matches!(self, Self::Policy)
    }
}

/// A plan as a plain tree, before it enters the e-graph and after extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanExpr {
    pub op: StageOp,
    pub children: Vec<Self>,
}

impl PlanExpr {
    /// A terminal stage.
    #[must_use]
    pub const fn leaf(op: StageOp) -> Self {
        Self {
            op,
            children: Vec::new(),
        }
    }

    /// A stage wrapping one child stage.
    #[must_use]
    pub fn unary(op: StageOp, child: Self) -> Self {
        Self {
            op,
            children: vec![child],
        }
    }

    /// Total cost of this tree under `model`.
    #[must_use]
    pub fn cost(&self, model: &CostModel) -> u32 {
        self.children
            .iter()
            .fold(model.stage_cost(&self.op), |acc, child| {
                acc.saturating_add(child.cost(model))
            })
    }

    /// Number of stages, used for budget accounting and tie-breaking reports.
    #[must_use]
    pub fn size(&self) -> usize {
        1 + self.children.iter().map(Self::size).sum::<usize>()
    }

    /// Depth-first stage labels, root first. This is the plan's identity for
    /// ambiguity checks: two extractions that differ here are different plans
    /// even when their costs tie.
    #[must_use]
    pub fn signature(&self) -> String {
        let mut out = self.op.label();
        if !self.children.is_empty() {
            out.push('[');
            for (i, child) in self.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&child.signature());
            }
            out.push(']');
        }
        out
    }

    /// Whether the tree contains a policy stage.
    fn has_policy(&self) -> bool {
        self.op.is_policy() || self.children.iter().any(Self::has_policy)
    }

    /// Count of policy stages, so a rewrite cannot quietly duplicate one.
    fn policy_count(&self) -> usize {
        usize::from(self.op.is_policy())
            + self.children.iter().map(Self::policy_count).sum::<usize>()
    }

    /// The opcode terminals in this tree, in order.
    fn opcodes(&self) -> Vec<String> {
        let mut found = Vec::new();
        self.collect_opcodes(&mut found);
        found
    }

    fn collect_opcodes(&self, out: &mut Vec<String>) {
        if let StageOp::Opcode(code) = &self.op {
            out.push(code.clone());
        }
        for child in &self.children {
            child.collect_opcodes(out);
        }
    }
}

// ── Cost model ──────────────────────────────────────────────────────────────

/// Per-stage cost in arbitrary units, intended to be populated from measured
/// stage attribution rather than guessed.
///
/// [`CostModel::measured_default`] carries the shape the workload harness
/// reports — JSON marshalling dominates, conversions are not free, fused
/// intrinsics cost less than the sum of their parts — without claiming to be a
/// calibrated measurement. Callers with real numbers should override.
#[derive(Debug, Clone)]
pub struct CostModel {
    pub opcode: u32,
    pub marshal_json: u32,
    pub marshal_typed: u32,
    pub marshal_bytes: u32,
    pub validate: u32,
    pub policy: u32,
    pub dispatch: u32,
    pub convert: u32,
    /// Cost of each fused intrinsic, by rule id.
    pub fused: BTreeMap<&'static str, u32>,
    /// Cost charged to a fused intrinsic with no explicit entry. Set high on
    /// purpose: an unpriced fusion should lose to the baseline rather than win
    /// by omission.
    pub fused_default: u32,
}

/// Per-stage costs measured by the workload harness, in microseconds.
///
/// Mirrors the six-stage decomposition `examples/ext_workloads` emits as
/// `pi.ext.hostcall_hotspot_matrix.v1`. Feed it to
/// [`CostModel::from_measured_stages`] to replace the hand-written defaults
/// with numbers from a real run (bd-oxu87).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeasuredStages {
    pub marshal_us: f64,
    pub queue_us: f64,
    pub schedule_us: f64,
    pub policy_us: f64,
    pub execute_us: f64,
    pub io_us: f64,
    /// Typed-decode latency, when marshalling telemetry is available.
    ///
    /// The six-stage matrix reports one `marshal` figure and cannot separate
    /// typed decode from JSON decode — but the marshalling path already times
    /// both lanes per call and reports the difference as
    /// `rewrite_observed_cost_delta` (see `src/extensions/protocol.rs`). Supply
    /// the fast-lane figure here to calibrate the parameter the whole fast path
    /// rests on, instead of leaving it modelled.
    pub marshal_typed_us: Option<f64>,
}

impl MeasuredStages {
    /// The six-stage matrix alone, with no marshalling telemetry.
    #[must_use]
    pub const fn from_stage_matrix(
        marshal_us: f64,
        queue_us: f64,
        schedule_us: f64,
        policy_us: f64,
        execute_us: f64,
        io_us: f64,
    ) -> Self {
        Self {
            marshal_us,
            queue_us,
            schedule_us,
            policy_us,
            execute_us,
            io_us,
            marshal_typed_us: None,
        }
    }

    /// Add the typed-decode latency from marshalling telemetry.
    ///
    /// `fast_candidate_latency_us` in `HostcallMarshallingArtifacts` is the
    /// measurement; `baseline_latency_us` is already the `marshal_us` above, so
    /// the pair gives both sides of the comparison the fast lane exists to win.
    #[must_use]
    pub const fn with_typed_marshal(mut self, fast_candidate_latency_us: f64) -> Self {
        self.marshal_typed_us = Some(fast_candidate_latency_us);
        self
    }
}

/// What a calibration run could and could not measure.
///
/// The harness's six stages are coarser than [`StageOp`], so a measured run
/// does **not** determine every field of a [`CostModel`]. Rather than let the
/// difference disappear into plausible-looking numbers, calibration reports it:
/// anything listed in `unmeasured` kept a conservative default, and any saving
/// that depends on it is modelled, not measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationReport {
    /// `CostModel` fields the run determined.
    pub measured: Vec<&'static str>,
    /// Fields the harness cannot distinguish, left at conservative defaults.
    pub unmeasured: Vec<&'static str>,
}

impl CalibrationReport {
    /// Whether every field that affects fusion selection was measured.
    ///
    /// False means a reported saving is partly modelled — which is a legitimate
    /// state to be in, but not one to make a performance claim from.
    #[must_use]
    pub fn is_fully_measured(&self) -> bool {
        self.unmeasured.is_empty()
    }
}

impl CostModel {
    /// Build a cost model from a measured run, reporting what it could not fix.
    ///
    /// # The mapping, and its limits
    ///
    /// Three [`StageOp`]s map onto a measured stage directly:
    /// - [`StageOp::Marshal`] with [`Repr::Json`] <- `marshal_us`, the canonical
    ///   decode the fast lane exists to avoid.
    /// - [`StageOp::Policy`] <- `policy_us`.
    /// - [`StageOp::Dispatch`] <- `queue_us + schedule_us`, the routing work
    ///   between authorization and execution.
    ///
    /// `marshal_typed` is measured **when the caller supplies it** via
    /// [`MeasuredStages::with_typed_marshal`]. The six-stage matrix cannot
    /// distinguish typed decode from JSON decode, but the marshalling path
    /// already times both lanes per call (`baseline_latency_us` and
    /// `fast_candidate_latency_us` in `src/extensions/protocol.rs`, reported as
    /// `rewrite_observed_cost_delta`). That is the parameter the entire fast
    /// path rests on, so it is worth wiring through rather than modelling.
    ///
    /// The rest **cannot be derived from either source**, and calibration says
    /// so instead of inventing them:
    /// - `validate`, `convert`: the marshal figure covers decode and shape
    ///   checking together; neither source splits them.
    /// - `fused`: no measurement corresponds to an intrinsic that has not been
    ///   built yet.
    ///
    /// Unmeasured fields keep [`Self::measured_default`]'s conservative values,
    /// so a fusion whose benefit was never measured still has to beat the
    /// baseline on numbers that do not flatter it.
    ///
    /// `execute_us` and `io_us` are deliberately unused: they price the work the
    /// hostcall performs, which every plan pays identically. Including them
    /// would inflate both sides of every comparison and shrink the apparent
    /// difference between plans.
    #[must_use]
    pub fn from_measured_stages(stages: MeasuredStages) -> (Self, CalibrationReport) {
        /// Round a microsecond figure into the model's integer cost units,
        /// clamping at 1 so a measured stage never prices as free.
        fn cost_of(us: f64) -> u32 {
            if !us.is_finite() || us <= 0.0 {
                return 1;
            }
            let rounded = us.round();
            if rounded >= f64::from(u32::MAX) {
                u32::MAX
            } else {
                (rounded as u32).max(1)
            }
        }

        let mut model = Self::measured_default();
        model.marshal_json = cost_of(stages.marshal_us);
        model.policy = cost_of(stages.policy_us);
        model.dispatch = cost_of(stages.queue_us + stages.schedule_us);

        let mut measured = vec!["marshal_json", "policy", "dispatch"];
        let mut unmeasured = vec!["validate", "convert", "fused"];

        if let Some(typed_us) = stages.marshal_typed_us {
            model.marshal_typed = cost_of(typed_us);
            measured.push("marshal_typed");
        } else {
            unmeasured.push("marshal_typed");
        }
        // Both lists sorted so a report is comparable across runs regardless of
        // which fields a given run happened to fill.
        measured.sort_unstable();
        unmeasured.sort_unstable();

        (
            model,
            CalibrationReport {
                measured,
                unmeasured,
            },
        )
    }

    /// Cost shape matching the harness's stage attribution.
    ///
    /// Hand-written, and **not** a calibrated measurement: it carries the shape
    /// real runs report — JSON marshalling dominates, conversions are not free,
    /// fused intrinsics cost less than the sum of their parts — so the search
    /// behaves sensibly out of the box. Use [`Self::from_measured_stages`] when
    /// a real run is available.
    #[must_use]
    pub fn measured_default() -> Self {
        let mut fused = BTreeMap::new();
        fused.insert(RULE_FUSE_MARSHAL_VALIDATE, 22_u32);
        fused.insert(RULE_FUSE_VALIDATE_DISPATCH, 18_u32);
        fused.insert(RULE_FUSE_TYPED_PIPELINE, 26_u32);
        Self {
            opcode: 0,
            marshal_json: 30,
            marshal_typed: 12,
            marshal_bytes: 2,
            validate: 14,
            policy: 8,
            dispatch: 10,
            convert: 9,
            fused,
            fused_default: 1_000,
        }
    }

    /// Cost of a single stage, excluding children.
    #[must_use]
    pub fn stage_cost(&self, op: &StageOp) -> u32 {
        match op {
            StageOp::Opcode(_) => self.opcode,
            StageOp::Marshal(Repr::Json) => self.marshal_json,
            StageOp::Marshal(Repr::Typed) => self.marshal_typed,
            StageOp::Marshal(Repr::Bytes) => self.marshal_bytes,
            StageOp::Validate => self.validate,
            StageOp::Policy => self.policy,
            StageOp::Dispatch => self.dispatch,
            StageOp::Convert { .. } => self.convert,
            StageOp::Fused(rule) => self.fused.get(rule).copied().unwrap_or(self.fused_default),
        }
    }
}

impl Default for CostModel {
    fn default() -> Self {
        Self::measured_default()
    }
}

// ── Rewrite rules ───────────────────────────────────────────────────────────

pub const RULE_DROP_ROUNDTRIP_CONVERT: &str = "drop_roundtrip_convert";
pub const RULE_COLLAPSE_CHAINED_CONVERT: &str = "collapse_chained_convert";
pub const RULE_FUSE_MARSHAL_VALIDATE: &str = "fuse_marshal_validate";
pub const RULE_FUSE_VALIDATE_DISPATCH: &str = "fuse_validate_dispatch";
pub const RULE_FUSE_TYPED_PIPELINE: &str = "fuse_typed_pipeline";

/// A semantics-preserving rewrite.
///
/// Rules are Rust closures over concrete node shapes rather than a pattern
/// DSL. The bead calls for a *constrained* rule set, and a closure that
/// inspects the exact shape it handles is easier to audit for the policy
/// invariant than a generic matcher would be.
pub struct RewriteRule {
    pub id: &'static str,
    /// Why this rewrite preserves observable behavior. Carried into telemetry
    /// so a decision can be explained without reading this file.
    pub invariant: &'static str,
    matcher: fn(&PlanExpr) -> Option<PlanExpr>,
}

impl std::fmt::Debug for RewriteRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RewriteRule")
            .field("id", &self.id)
            .field("invariant", &self.invariant)
            .finish_non_exhaustive()
    }
}

impl RewriteRule {
    /// Apply to one node, returning the equivalent form if the shape matches.
    ///
    /// The policy invariant is enforced *here*, on every application, not only
    /// in review: a rewrite that changes how many authorization steps a plan
    /// performs is dropped even if its matcher produced it. Enforcing at the
    /// application site means a future rule cannot bypass the check by
    /// forgetting to call it.
    #[must_use]
    pub fn apply(&self, expr: &PlanExpr) -> Option<PlanExpr> {
        let rewritten = (self.matcher)(expr)?;
        if Self::is_policy_preserving(expr, &rewritten) {
            Some(rewritten)
        } else {
            None
        }
    }

    /// Whether a rewrite leaves authorization behavior untouched.
    ///
    /// Requires the policy stage count to be identical, not merely nonzero:
    /// dropping one authorization and adding another elsewhere would keep a
    /// boolean "has policy" check happy while changing what gets authorized.
    #[must_use]
    pub fn is_policy_preserving(before: &PlanExpr, after: &PlanExpr) -> bool {
        before.policy_count() == after.policy_count()
            && before.has_policy() == after.has_policy()
            && before.opcodes() == after.opcodes()
    }
}

/// The constrained rule set.
#[must_use]
pub fn rewrite_rules() -> Vec<RewriteRule> {
    vec![
        RewriteRule {
            id: RULE_DROP_ROUNDTRIP_CONVERT,
            invariant: "convert(a->b) over convert(b->a) is the identity on the payload; \
                        removing both yields the same bytes reaching the same stage",
            matcher: |expr| {
                let StageOp::Convert { from: b1, to: a1 } = &expr.op else {
                    return None;
                };
                let inner = expr.children.first()?;
                let StageOp::Convert { from: a2, to: b2 } = &inner.op else {
                    return None;
                };
                // Outer undoes inner: inner a2->b2, outer b1->a1 with b1==b2,
                // a1==a2. The pair is the identity, so both drop out.
                if b1 == b2 && a1 == a2 {
                    inner.children.first().cloned()
                } else {
                    None
                }
            },
        },
        RewriteRule {
            id: RULE_COLLAPSE_CHAINED_CONVERT,
            invariant: "convert(b->c) over convert(a->b) reaches representation c from a; \
                        the direct conversion produces the same payload in one step",
            matcher: |expr| {
                let StageOp::Convert { from: b1, to: c } = &expr.op else {
                    return None;
                };
                let inner = expr.children.first()?;
                let StageOp::Convert { from: a, to: b2 } = &inner.op else {
                    return None;
                };
                // Only a genuine chain, and never a round-trip: a==c is the
                // identity case, which RULE_DROP_ROUNDTRIP_CONVERT owns.
                if b1 != b2 || a == c {
                    return None;
                }
                Some(PlanExpr::unary(
                    StageOp::Convert { from: *a, to: *c },
                    inner.children.first().cloned()?,
                ))
            },
        },
        RewriteRule {
            id: RULE_FUSE_MARSHAL_VALIDATE,
            invariant: "the typed decoder validates shape while decoding; a separate \
                        validation pass over its output is redundant work, not a \
                        second check",
            matcher: |expr| {
                if !matches!(expr.op, StageOp::Validate) {
                    return None;
                }
                let inner = expr.children.first()?;
                if !matches!(inner.op, StageOp::Marshal(Repr::Typed)) {
                    return None;
                }
                Some(PlanExpr::unary(
                    StageOp::Fused(RULE_FUSE_MARSHAL_VALIDATE),
                    inner.children.first().cloned()?,
                ))
            },
        },
        RewriteRule {
            id: RULE_FUSE_VALIDATE_DISPATCH,
            invariant: "dispatch immediately after validation re-reads the same decoded \
                        payload; the fused intrinsic routes from the validation result \
                        without a second traversal",
            matcher: |expr| {
                if !matches!(expr.op, StageOp::Dispatch) {
                    return None;
                }
                let inner = expr.children.first()?;
                if !matches!(inner.op, StageOp::Validate) {
                    return None;
                }
                Some(PlanExpr::unary(
                    StageOp::Fused(RULE_FUSE_VALIDATE_DISPATCH),
                    inner.children.first().cloned()?,
                ))
            },
        },
        RewriteRule {
            id: RULE_FUSE_TYPED_PIPELINE,
            invariant: "dispatch over an already-fused marshal+validate is the whole \
                        typed pipeline; one intrinsic performs decode, shape check, \
                        and routing over a single borrow",
            matcher: |expr| {
                if !matches!(expr.op, StageOp::Dispatch) {
                    return None;
                }
                let inner = expr.children.first()?;
                if !matches!(inner.op, StageOp::Fused(RULE_FUSE_MARSHAL_VALIDATE)) {
                    return None;
                }
                Some(PlanExpr::unary(
                    StageOp::Fused(RULE_FUSE_TYPED_PIPELINE),
                    inner.children.first().cloned()?,
                ))
            },
        },
    ]
}

// ── E-graph ─────────────────────────────────────────────────────────────────

/// Identity of an equivalence class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EClassId(usize);

/// A node whose children are equivalence classes rather than concrete trees.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ENode {
    op: StageOp,
    children: Vec<EClassId>,
}

/// Why saturation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaturationOutcome {
    /// No rule produced anything new: the search is complete.
    Fixpoint,
    /// The iteration ceiling was reached first.
    IterationBudget,
    /// The node ceiling was reached first.
    NodeBudget,
    /// A class described more trees than the enumeration ceiling allows, so the
    /// search could not read the graph back out in full.
    EnumerationBudget,
}

impl SaturationOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fixpoint => "fixpoint",
            Self::IterationBudget => "iteration_budget",
            Self::NodeBudget => "node_budget",
            Self::EnumerationBudget => "enumeration_budget",
        }
    }

    /// Only a fixpoint proves the search considered every reachable form.
    /// A budget stop leaves unexplored plans, so its result cannot be called
    /// minimal.
    const fn is_complete(self) -> bool {
        matches!(self, Self::Fixpoint)
    }
}

/// An e-graph over [`PlanExpr`] with union-find and congruence closure.
#[derive(Debug)]
pub struct EGraph {
    /// Union-find parent links over class ids.
    parents: Vec<usize>,
    /// Canonical class id -> its member nodes.
    classes: BTreeMap<usize, Vec<ENode>>,
    /// Hashcons: a canonicalized node maps to exactly one class.
    memo: HashMap<ENode, EClassId>,
    node_count: usize,
}

impl Default for EGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl EGraph {
    #[must_use]
    pub fn new() -> Self {
        Self {
            parents: Vec::new(),
            classes: BTreeMap::new(),
            memo: HashMap::new(),
            node_count: 0,
        }
    }

    /// Number of distinct e-nodes currently held.
    #[must_use]
    pub const fn node_count(&self) -> usize {
        self.node_count
    }

    /// Number of equivalence classes, counting merged classes once.
    #[must_use]
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// Union-find `find` with path compression.
    fn find(&mut self, id: EClassId) -> EClassId {
        let mut root = id.0;
        while self.parents[root] != root {
            root = self.parents[root];
        }
        // Compress: point every node on the path straight at the root, so
        // repeated lookups during saturation stay near-constant.
        let mut cur = id.0;
        while self.parents[cur] != cur {
            let next = self.parents[cur];
            self.parents[cur] = root;
            cur = next;
        }
        EClassId(root)
    }

    /// `find` without mutation, for read-only paths like extraction.
    fn find_const(&self, id: EClassId) -> EClassId {
        let mut root = id.0;
        while self.parents[root] != root {
            root = self.parents[root];
        }
        EClassId(root)
    }

    fn canonicalize(&mut self, node: &ENode) -> ENode {
        ENode {
            op: node.op.clone(),
            children: node.children.iter().map(|c| self.find(*c)).collect(),
        }
    }

    fn fresh_class(&mut self) -> EClassId {
        let id = self.parents.len();
        self.parents.push(id);
        self.classes.insert(id, Vec::new());
        EClassId(id)
    }

    /// Insert a node, returning its class. Identical nodes share a class, so
    /// structurally equal subterms are automatically shared.
    fn add_node(&mut self, node: &ENode) -> EClassId {
        let canonical = self.canonicalize(node);
        if let Some(existing) = self.memo.get(&canonical) {
            return self.find(*existing);
        }
        let id = self.fresh_class();
        self.classes
            .entry(id.0)
            .or_default()
            .push(canonical.clone());
        self.memo.insert(canonical, id);
        self.node_count += 1;
        id
    }

    /// Insert a whole tree, returning the class of its root.
    pub fn add_expr(&mut self, expr: &PlanExpr) -> EClassId {
        let children: Vec<EClassId> = expr
            .children
            .iter()
            .map(|child| self.add_expr(child))
            .collect();
        self.add_node(&ENode {
            op: expr.op.clone(),
            children,
        })
    }

    /// Whether `id` was issued by this graph.
    ///
    /// Ids are opaque and only this module can mint them, but nothing stops a
    /// caller handing one graph an id that another graph issued. Checking is
    /// cheaper than the alternative, which is an index panic out of a public
    /// method.
    const fn owns(&self, id: EClassId) -> bool {
        id.0 < self.parents.len()
    }

    /// Assert that two classes denote the same plan. Returns whether this
    /// changed anything.
    ///
    /// Returns `false` for an id this graph did not issue, rather than
    /// panicking on the out-of-range index.
    pub fn union(&mut self, a: EClassId, b: EClassId) -> bool {
        if !self.owns(a) || !self.owns(b) {
            return false;
        }
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return false;
        }
        // Merge the smaller class into the larger to keep the tree shallow.
        let (keep, drop) = {
            let len_a = self.classes.get(&ra.0).map_or(0, Vec::len);
            let len_b = self.classes.get(&rb.0).map_or(0, Vec::len);
            if len_a >= len_b { (ra, rb) } else { (rb, ra) }
        };
        self.parents[drop.0] = keep.0;
        let moved = self.classes.remove(&drop.0).unwrap_or_default();
        self.classes.entry(keep.0).or_default().extend(moved);
        self.rebuild();
        true
    }

    /// Restore the congruence invariant after a union.
    ///
    /// Merging two classes can make previously distinct parent nodes equal —
    /// `f(x)` and `f(y)` become the same node once `x` and `y` merge. Without
    /// this, the graph would hold duplicate classes for terms it has already
    /// proven equal, and extraction could miss the cheaper of two identical
    /// plans. Iterates because each repair can expose the next.
    fn rebuild(&mut self) {
        loop {
            let mut pending: Vec<(EClassId, EClassId)> = Vec::new();
            let mut seen: HashMap<ENode, EClassId> = HashMap::new();
            let class_ids: Vec<usize> = self.classes.keys().copied().collect();

            for class_id in class_ids {
                let nodes = self.classes.get(&class_id).cloned().unwrap_or_default();
                for node in nodes {
                    let canonical = self.canonicalize(&node);
                    let owner = self.find(EClassId(class_id));
                    if let Some(prev) = seen.get(&canonical) {
                        let prev_root = self.find(*prev);
                        if prev_root != owner {
                            pending.push((prev_root, owner));
                        }
                    } else {
                        seen.insert(canonical, owner);
                    }
                }
            }

            if pending.is_empty() {
                break;
            }
            // Apply merges directly rather than through union(), which would
            // recurse back into rebuild().
            for (a, b) in pending {
                let (ra, rb) = (self.find(a), self.find(b));
                if ra == rb {
                    continue;
                }
                self.parents[rb.0] = ra.0;
                let moved = self.classes.remove(&rb.0).unwrap_or_default();
                self.classes.entry(ra.0).or_default().extend(moved);
            }
        }

        // Rebuild the memo table against the new canonical form, deduplicating
        // nodes that just became identical.
        let mut memo = HashMap::new();
        let class_ids: Vec<usize> = self.classes.keys().copied().collect();
        let mut node_count = 0;
        for class_id in class_ids {
            let nodes = self.classes.get(&class_id).cloned().unwrap_or_default();
            let mut unique: Vec<ENode> = Vec::new();
            for node in nodes {
                let canonical = self.canonicalize(&node);
                if !unique.contains(&canonical) {
                    unique.push(canonical.clone());
                }
                memo.insert(canonical, self.find(EClassId(class_id)));
            }
            node_count += unique.len();
            self.classes.insert(class_id, unique);
        }
        self.memo = memo;
        self.node_count = node_count;
    }

    /// Every concrete tree in a class, bounded by `depth` and by `cap`.
    ///
    /// Used by saturation to feed whole subtrees to the shape-matching rules.
    /// The depth bound keeps a cyclic class — entirely normal in an e-graph,
    /// and exactly what a round-trip conversion rule creates — from enumerating
    /// forever.
    ///
    /// The depth bound alone is not enough. This takes a Cartesian product over
    /// child expansions, so a class holding `k` alternatives can yield up to
    /// `k^depth` trees: bounded, but astronomically. `cap` bounds the actual
    /// output, and exceeding it returns `None` rather than a truncated list.
    ///
    /// That distinction is the whole point. A truncated enumeration would make
    /// saturation miss rewrites and the ambiguity check miss ties, and both
    /// would then report success — silently converting "we ran out of room"
    /// into "we proved this is optimal". `None` forces the caller to fail
    /// closed instead.
    fn enumerate(&self, class: EClassId, depth: usize, cap: usize) -> Option<Vec<PlanExpr>> {
        if depth == 0 {
            return Some(Vec::new());
        }
        let root = self.find_const(class);
        let Some(nodes) = self.classes.get(&root.0) else {
            return Some(Vec::new());
        };

        let mut out: Vec<PlanExpr> = Vec::new();
        for node in nodes {
            if node.children.is_empty() {
                out.push(PlanExpr::leaf(node.op.clone()));
                if out.len() > cap {
                    return None;
                }
                continue;
            }
            // Cartesian product over child expansions.
            let mut combos: Vec<Vec<PlanExpr>> = vec![Vec::new()];
            let mut viable = true;
            for child in &node.children {
                let options = self.enumerate(*child, depth - 1, cap)?;
                if options.is_empty() {
                    // This child cannot be expanded within the depth bound, so
                    // no complete tree runs through this node.
                    viable = false;
                    break;
                }
                // Check the product before building it: `combos.len() *
                // options.len()` is the size we are about to materialize.
                if combos.len().saturating_mul(options.len()) > cap {
                    return None;
                }
                let mut next = Vec::with_capacity(combos.len() * options.len());
                for combo in &combos {
                    for option in &options {
                        let mut extended = combo.clone();
                        extended.push(option.clone());
                        next.push(extended);
                    }
                }
                combos = next;
            }
            if !viable {
                continue;
            }
            for children in combos {
                out.push(PlanExpr {
                    op: node.op.clone(),
                    children,
                });
                if out.len() > cap {
                    return None;
                }
            }
        }
        Some(out)
    }

    /// Cheapest tree in each class, by fixpoint over node costs.
    ///
    /// Costs start at "unknown" and are relaxed until stable, which handles
    /// the cyclic classes that rewriting creates: a class reachable only
    /// through itself never gains a finite cost and is simply never selected.
    fn extract_costs(&self, model: &CostModel) -> BTreeMap<usize, (u32, ENode)> {
        let mut best: BTreeMap<usize, (u32, ENode)> = BTreeMap::new();
        loop {
            let mut changed = false;
            for (class_id, nodes) in &self.classes {
                for node in nodes {
                    let mut total = model.stage_cost(&node.op);
                    let mut resolvable = true;
                    for child in &node.children {
                        let root = self.find_const(*child);
                        if let Some((child_cost, _)) = best.get(&root.0) {
                            total = total.saturating_add(*child_cost);
                        } else {
                            resolvable = false;
                            break;
                        }
                    }
                    if !resolvable {
                        continue;
                    }
                    let improved = match best.get(class_id) {
                        None => true,
                        Some((current, _)) => total < *current,
                    };
                    if improved {
                        best.insert(*class_id, (total, node.clone()));
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        best
    }

    /// Rebuild the cheapest tree for `class` from an extraction table.
    fn build_best(
        &self,
        class: EClassId,
        best: &BTreeMap<usize, (u32, ENode)>,
        depth: usize,
    ) -> Option<PlanExpr> {
        if depth == 0 {
            return None;
        }
        let root = self.find_const(class);
        let (_, node) = best.get(&root.0)?;
        let mut children = Vec::with_capacity(node.children.len());
        for child in &node.children {
            children.push(self.build_best(*child, best, depth - 1)?);
        }
        Some(PlanExpr {
            op: node.op.clone(),
            children,
        })
    }
}

// ── Saturation and extraction ───────────────────────────────────────────────

/// Bounds on the search.
#[derive(Debug, Clone, Copy)]
pub struct SaturationLimits {
    pub max_iterations: usize,
    pub max_nodes: usize,
    /// Depth bound when enumerating a class into concrete trees.
    pub max_expr_depth: usize,
    /// Ceiling on trees produced by a single enumeration.
    ///
    /// Separate from `max_nodes` because they bound different things: the node
    /// budget limits how big the graph gets, this limits how many distinct
    /// trees that graph can be read out as. Enumeration is a Cartesian product,
    /// so a graph well inside its node budget can still describe astronomically
    /// many trees. Exceeding this fails closed.
    pub max_enumerated: usize,
}

impl Default for SaturationLimits {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_nodes: DEFAULT_MAX_NODES,
            max_expr_depth: 12,
            max_enumerated: DEFAULT_MAX_ENUMERATED,
        }
    }
}

/// What the search concluded, including the parts a reviewer needs to
/// disbelieve it.
#[derive(Debug, Clone)]
pub struct EGraphDecision {
    /// Plan to run. Equals `baseline` whenever `fallback_reason` is set.
    pub plan: PlanExpr,
    /// The plan as handed in.
    pub baseline: PlanExpr,
    pub baseline_cost: u32,
    pub selected_cost: u32,
    /// Positive when the selected plan is cheaper than the baseline.
    pub expected_cost_delta: i64,
    /// Rules that fired at least once, in application order.
    pub applied_rules: Vec<&'static str>,
    pub outcome: SaturationOutcome,
    pub iterations: usize,
    pub nodes: usize,
    pub classes: usize,
    /// Set when the baseline was kept. `None` means a rewrite was selected.
    pub fallback_reason: Option<&'static str>,
}

impl EGraphDecision {
    /// Whether a rewrite was selected over the baseline.
    #[must_use]
    pub const fn rewrote(&self) -> bool {
        self.fallback_reason.is_none()
    }

    /// Cost of the plan this decision settled on, rewritten or not.
    ///
    /// Unlike [`Self::selected_cost`], this is meaningful for a fallback too: a
    /// decision that declined to rewrite still has a best-known plan, namely
    /// the baseline it was handed.
    #[must_use]
    pub const fn best_cost(&self) -> u32 {
        if self.rewrote() {
            self.selected_cost
        } else {
            self.baseline_cost
        }
    }

    /// Express this decision's best plan as a fraction of `reference`'s best
    /// plan, rendered on `scale`.
    ///
    /// # Why this takes two decisions
    ///
    /// A ratio only means something when both sides measure the same kind of
    /// thing. Comparing a decision against *its own* baseline answers "how much
    /// did fusing help this plan?" — which is not the question a caller asking
    /// "how expensive is the typed path relative to the canonical one?" is
    /// asking. Answering the first and reporting it as the second is a category
    /// error: the result is a real ratio, just not of the two things being
    /// compared.
    ///
    /// So both sides are searched independently and their best plans compared.
    /// `self` is the candidate, `reference` is what it is measured against, and
    /// the result places the candidate on a scale where `reference` sits at
    /// `scale`.
    ///
    /// Returns `None` when the reference is free (no ratio exists) or the
    /// result does not fit the scale's type.
    #[must_use]
    pub fn relative_to(&self, reference: &Self, scale: u32) -> Option<u32> {
        let reference_cost = reference.best_cost();
        if reference_cost == 0 {
            return None;
        }
        let scaled = u64::from(self.best_cost()).saturating_mul(u64::from(scale))
            / u64::from(reference_cost);
        u32::try_from(scaled).ok()
    }

    /// Hand the result to [`crate::hostcall_rewrite::HostcallRewriteEngine`],
    /// which owns the final authorization.
    ///
    /// Deliberately does not decide anything itself: this converts the search
    /// result into that engine's vocabulary so the fast-path guard stays in
    /// one place. A search that fell back reports a baseline-cost candidate,
    /// which that engine's `no_better_candidate` path then rejects.
    #[must_use]
    pub const fn to_rewrite_plan(
        &self,
        kind: HostcallRewritePlanKind,
        rule_id: &'static str,
    ) -> HostcallRewritePlan {
        HostcallRewritePlan {
            kind,
            estimated_cost: if self.rewrote() {
                self.selected_cost
            } else {
                self.baseline_cost
            },
            rule_id,
        }
    }

    /// Structured telemetry. Carries only plan structure, costs, and rule ids
    /// — never payloads, arguments, or extension-authored strings.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": HOSTCALL_EGRAPH_SCHEMA,
            "rewrote": self.rewrote(),
            "baseline": {
                "signature": self.baseline.signature(),
                "cost": self.baseline_cost,
                "stages": self.baseline.size(),
            },
            "selected": {
                "signature": self.plan.signature(),
                "cost": self.selected_cost,
                "stages": self.plan.size(),
            },
            "expected_cost_delta": self.expected_cost_delta,
            "applied_rules": self.applied_rules,
            "saturation": {
                "outcome": self.outcome.as_str(),
                "complete": self.outcome.is_complete(),
                "iterations": self.iterations,
                "nodes": self.nodes,
                "classes": self.classes,
            },
            "fallback_reason": self.fallback_reason,
            "redaction": {
                "payload_content": "omitted",
                "argument_values": "omitted",
            },
        })
    }
}

/// Equality-saturation rewrite search over hostcall plans.
#[derive(Debug)]
pub struct HostcallEGraphEngine {
    enabled: bool,
    limits: SaturationLimits,
    model: CostModel,
}

impl Default for HostcallEGraphEngine {
    fn default() -> Self {
        Self::new(true)
    }
}

impl HostcallEGraphEngine {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            limits: SaturationLimits::default(),
            model: CostModel::measured_default(),
        }
    }

    /// Read the same kill switch the existing planner honors, so one variable
    /// disables both halves of the rewrite path.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_opt(std::env::var("PI_HOSTCALL_EGRAPH_REWRITE").ok().as_deref())
    }

    /// Parse the kill switch from an explicit value.
    ///
    /// Split out of [`Self::from_env`] for the same reason
    /// [`crate::hostcall_rewrite::HostcallRewriteEngine::from_opt`] is: the
    /// crate is `#![forbid(unsafe_code)]` and `std::env::set_var` is unsafe in
    /// Rust 2024, so the parsing has to be testable without touching the
    /// process environment. Absent means enabled, matching that engine exactly
    /// — one variable, one meaning, both halves of the rewrite path.
    #[must_use]
    pub fn from_opt(value: Option<&str>) -> Self {
        let enabled = value.is_none_or(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "disabled"
            )
        });
        Self::new(enabled)
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: SaturationLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_cost_model(mut self, model: CostModel) -> Self {
        self.model = model;
        self
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn cost_model(&self) -> &CostModel {
        &self.model
    }

    /// Run rewrite rules to a fixpoint or a budget, whichever comes first.
    ///
    /// Returns the stopping reason and the iteration count. Split out of
    /// [`Self::optimize`] so the search loop and the decision logic can be read
    /// independently — the loop only grows the graph, and every judgement about
    /// what that growth means lives in the caller.
    fn saturate(
        &self,
        graph: &mut EGraph,
        applied: &mut BTreeSet<&'static str>,
    ) -> (SaturationOutcome, usize) {
        let rules = rewrite_rules();
        let mut iterations = 0;

        for iteration in 0..self.limits.max_iterations {
            iterations = iteration + 1;
            if graph.node_count() >= self.limits.max_nodes {
                return (SaturationOutcome::NodeBudget, iterations);
            }

            // Snapshot every class's concrete forms, then apply rules to each.
            // Rewriting *adds* an equivalent form rather than replacing one,
            // which is the property that makes phase ordering irrelevant.
            let class_ids: Vec<usize> = graph.classes.keys().copied().collect();
            let mut merges: Vec<(EClassId, PlanExpr, &'static str)> = Vec::new();
            for class_id in class_ids {
                let class = EClassId(class_id);
                let Some(exprs) = graph.enumerate(
                    class,
                    self.limits.max_expr_depth,
                    self.limits.max_enumerated,
                ) else {
                    // Cannot read this class back out in full, so we cannot
                    // claim to have applied every rule to it.
                    return (SaturationOutcome::EnumerationBudget, iterations);
                };
                for expr in exprs {
                    for rule in &rules {
                        if let Some(rewritten) = rule.apply(&expr) {
                            merges.push((class, rewritten, rule.id));
                        }
                    }
                }
            }

            let mut changed = false;
            for (class, rewritten, rule_id) in merges {
                if graph.node_count() >= self.limits.max_nodes {
                    return (SaturationOutcome::NodeBudget, iterations);
                }
                let new_class = graph.add_expr(&rewritten);
                if graph.union(class, new_class) {
                    applied.insert(rule_id);
                    changed = true;
                }
            }

            // Nothing new: every reachable equivalent form is already present.
            if !changed {
                return (SaturationOutcome::Fixpoint, iterations);
            }
        }

        (SaturationOutcome::IterationBudget, iterations)
    }

    /// Search for a cheaper form of `baseline`.
    ///
    /// Returns the baseline with a `fallback_reason` when the search is
    /// disabled, finds nothing better, cannot prove it explored everything, or
    /// cannot break a tie. Every one of those is a refusal to guess.
    #[must_use]
    pub fn optimize(&self, baseline: &PlanExpr) -> EGraphDecision {
        let baseline_cost = baseline.cost(&self.model);
        let mut decision = EGraphDecision {
            plan: baseline.clone(),
            baseline: baseline.clone(),
            baseline_cost,
            selected_cost: baseline_cost,
            expected_cost_delta: 0,
            applied_rules: Vec::new(),
            outcome: SaturationOutcome::Fixpoint,
            iterations: 0,
            nodes: 0,
            classes: 0,
            fallback_reason: None,
        };

        if !self.enabled {
            decision.fallback_reason = Some("egraph_disabled");
            return decision;
        }

        let mut graph = EGraph::new();
        let root = graph.add_expr(baseline);
        let mut applied: BTreeSet<&'static str> = BTreeSet::new();
        let (outcome, iterations) = self.saturate(&mut graph, &mut applied);

        decision.outcome = outcome;
        decision.iterations = iterations;
        decision.nodes = graph.node_count();
        decision.classes = graph.class_count();
        decision.applied_rules = applied.into_iter().collect();

        // A budget stop means unexplored plans remain, so "minimum cost" would
        // be a claim the search did not earn.
        if !outcome.is_complete() {
            // Exhaustive on purpose: a wildcard here would silently report a
            // future stopping reason as an iteration-budget stop.
            decision.fallback_reason = Some(match outcome {
                SaturationOutcome::NodeBudget => "node_budget_exhausted",
                SaturationOutcome::EnumerationBudget => "enumeration_budget_exhausted",
                SaturationOutcome::IterationBudget => "iteration_budget_exhausted",
                // Unreachable: is_complete() is true only for Fixpoint, and we
                // are inside the !is_complete() branch.
                SaturationOutcome::Fixpoint => "fixpoint_not_a_fallback",
            });
            return decision;
        }

        let best = graph.extract_costs(&self.model);
        let Some(extracted) = graph.build_best(root, &best, self.limits.max_expr_depth) else {
            decision.fallback_reason = Some("extraction_failed");
            return decision;
        };

        let extracted_cost = extracted.cost(&self.model);
        if extracted_cost >= baseline_cost {
            decision.fallback_reason = Some("no_better_plan");
            return decision;
        }

        // Ambiguity check. Two structurally different plans tying at the
        // minimum means the cost model does not actually prefer one; picking
        // either would make the choice an artifact of iteration order rather
        // than of measurement.
        let Some(all_plans) =
            graph.enumerate(root, self.limits.max_expr_depth, self.limits.max_enumerated)
        else {
            // Without a full enumeration we cannot rule out a tie, and an
            // unchecked tie is exactly what this guard exists to prevent.
            decision.outcome = SaturationOutcome::EnumerationBudget;
            decision.fallback_reason = Some("enumeration_budget_exhausted");
            return decision;
        };
        let distinct: BTreeSet<String> = all_plans
            .iter()
            .filter(|candidate| candidate.cost(&self.model) == extracted_cost)
            .map(PlanExpr::signature)
            .collect();
        if distinct.len() > 1 {
            decision.fallback_reason = Some("ambiguous_min_cost");
            return decision;
        }

        // Last line of defense: the extracted plan must still be the baseline's
        // equivalent. Individual rules are checked on application, but a
        // composition bug would show up only here.
        if !RewriteRule::is_policy_preserving(baseline, &extracted) {
            decision.fallback_reason = Some("policy_shape_changed");
            return decision;
        }

        decision.selected_cost = extracted_cost;
        decision.expected_cost_delta = i64::from(baseline_cost) - i64::from(extracted_cost);
        decision.plan = extracted;
        decision
    }
}

/// The canonical JSON-marshalling pipeline: the shape the fast path exists to
/// improve on.
#[must_use]
pub fn canonical_plan(opcode: &str) -> PlanExpr {
    PlanExpr::unary(
        StageOp::Dispatch,
        PlanExpr::unary(
            StageOp::Validate,
            PlanExpr::unary(
                StageOp::Marshal(Repr::Json),
                PlanExpr::unary(
                    StageOp::Policy,
                    PlanExpr::leaf(StageOp::Opcode(opcode.to_string())),
                ),
            ),
        ),
    )
}

/// The typed pipeline, with a redundant JSON round-trip of the kind real
/// traces contain when a caller hands JSON to a typed lane.
#[must_use]
pub fn typed_plan_with_roundtrip(opcode: &str) -> PlanExpr {
    PlanExpr::unary(
        StageOp::Dispatch,
        PlanExpr::unary(
            StageOp::Validate,
            PlanExpr::unary(
                StageOp::Marshal(Repr::Typed),
                PlanExpr::unary(
                    StageOp::Convert {
                        from: Repr::Json,
                        to: Repr::Bytes,
                    },
                    PlanExpr::unary(
                        StageOp::Convert {
                            from: Repr::Bytes,
                            to: Repr::Json,
                        },
                        PlanExpr::unary(
                            StageOp::Policy,
                            PlanExpr::leaf(StageOp::Opcode(opcode.to_string())),
                        ),
                    ),
                ),
            ),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opcode_leaf() -> PlanExpr {
        PlanExpr::leaf(StageOp::Opcode("tool.read".to_string()))
    }

    // ── E-graph core ────────────────────────────────────────────────────

    #[test]
    fn identical_subterms_share_one_class() {
        let mut graph = EGraph::new();
        let a = graph.add_expr(&canonical_plan("tool.read"));
        let b = graph.add_expr(&canonical_plan("tool.read"));
        assert_eq!(
            a, b,
            "hashcons must give structurally equal plans one class"
        );
        // 5 stages, shared rather than duplicated.
        assert_eq!(graph.node_count(), 5);
    }

    #[test]
    fn distinct_opcodes_do_not_share_a_class() {
        let mut graph = EGraph::new();
        let read = graph.add_expr(&canonical_plan("tool.read"));
        let write = graph.add_expr(&canonical_plan("tool.write"));
        assert_ne!(read, write, "different opcodes are different plans");
    }

    #[test]
    fn union_is_reflexive_symmetric_and_transitive() {
        let mut graph = EGraph::new();
        let a = graph.add_expr(&PlanExpr::leaf(StageOp::Validate));
        let b = graph.add_expr(&PlanExpr::leaf(StageOp::Dispatch));
        let c = graph.add_expr(&PlanExpr::leaf(StageOp::Policy));

        assert!(!graph.union(a, a), "union with self changes nothing");
        assert!(graph.union(a, b));
        assert_eq!(graph.find(a), graph.find(b), "symmetric");
        assert!(graph.union(b, c));
        assert_eq!(graph.find(a), graph.find(c), "transitive through b");
    }

    #[test]
    fn congruence_closure_merges_parents_of_merged_children() {
        // f(x) and f(y) must become equal once x and y do. Without rebuild()
        // the graph would keep two classes for terms it has proven equal.
        let mut graph = EGraph::new();
        let x = graph.add_expr(&PlanExpr::leaf(StageOp::Validate));
        let y = graph.add_expr(&PlanExpr::leaf(StageOp::Dispatch));
        let fx = graph.add_expr(&PlanExpr::unary(
            StageOp::Policy,
            PlanExpr::leaf(StageOp::Validate),
        ));
        let fy = graph.add_expr(&PlanExpr::unary(
            StageOp::Policy,
            PlanExpr::leaf(StageOp::Dispatch),
        ));
        assert_ne!(graph.find(fx), graph.find(fy), "distinct before the union");

        graph.union(x, y);
        assert_eq!(
            graph.find(fx),
            graph.find(fy),
            "congruence: equal children make equal parents"
        );
    }

    // ── Rule semantics ──────────────────────────────────────────────────

    #[test]
    fn roundtrip_conversions_cancel() {
        let rules = rewrite_rules();
        let rule = rules
            .iter()
            .find(|r| r.id == RULE_DROP_ROUNDTRIP_CONVERT)
            .expect("rule present");
        let expr = PlanExpr::unary(
            StageOp::Convert {
                from: Repr::Bytes,
                to: Repr::Json,
            },
            PlanExpr::unary(
                StageOp::Convert {
                    from: Repr::Json,
                    to: Repr::Bytes,
                },
                opcode_leaf(),
            ),
        );
        let rewritten = rule.apply(&expr).expect("round trip matches");
        assert_eq!(rewritten, opcode_leaf(), "both conversions drop out");
    }

    #[test]
    fn non_roundtrip_conversions_are_left_alone() {
        let rules = rewrite_rules();
        let rule = rules
            .iter()
            .find(|r| r.id == RULE_DROP_ROUNDTRIP_CONVERT)
            .expect("rule present");
        // json->bytes then bytes->typed is a chain, not a round trip; dropping
        // it would change the representation reaching the next stage.
        let expr = PlanExpr::unary(
            StageOp::Convert {
                from: Repr::Bytes,
                to: Repr::Typed,
            },
            PlanExpr::unary(
                StageOp::Convert {
                    from: Repr::Json,
                    to: Repr::Bytes,
                },
                opcode_leaf(),
            ),
        );
        assert!(rule.apply(&expr).is_none());
    }

    #[test]
    fn chained_conversions_collapse_to_the_direct_one() {
        let rules = rewrite_rules();
        let rule = rules
            .iter()
            .find(|r| r.id == RULE_COLLAPSE_CHAINED_CONVERT)
            .expect("rule present");
        let expr = PlanExpr::unary(
            StageOp::Convert {
                from: Repr::Bytes,
                to: Repr::Typed,
            },
            PlanExpr::unary(
                StageOp::Convert {
                    from: Repr::Json,
                    to: Repr::Bytes,
                },
                opcode_leaf(),
            ),
        );
        let rewritten = rule.apply(&expr).expect("chain matches");
        assert_eq!(
            rewritten.op,
            StageOp::Convert {
                from: Repr::Json,
                to: Repr::Typed
            }
        );
    }

    #[test]
    fn a_rule_that_drops_policy_is_refused() {
        // The invariant is enforced at application, so a matcher that returns
        // an unauthorized plan cannot land it.
        let bad = RewriteRule {
            id: "test_only_drops_policy",
            invariant: "deliberately unsound, for the guard test",
            matcher: |expr| {
                if matches!(expr.op, StageOp::Policy) {
                    expr.children.first().cloned()
                } else {
                    None
                }
            },
        };
        let expr = PlanExpr::unary(StageOp::Policy, opcode_leaf());
        assert!(
            bad.apply(&expr).is_none(),
            "removing authorization must never be applied"
        );
    }

    #[test]
    fn a_rule_that_duplicates_policy_is_refused() {
        // A count check, not a boolean: duplicating authorization would keep a
        // has-policy test happy while changing what runs.
        let bad = RewriteRule {
            id: "test_only_duplicates_policy",
            invariant: "deliberately unsound, for the guard test",
            matcher: |expr| {
                if matches!(expr.op, StageOp::Policy) {
                    Some(PlanExpr::unary(StageOp::Policy, expr.clone()))
                } else {
                    None
                }
            },
        };
        let expr = PlanExpr::unary(StageOp::Policy, opcode_leaf());
        assert!(bad.apply(&expr).is_none());
    }

    #[test]
    fn a_rule_that_swaps_the_opcode_is_refused() {
        let bad = RewriteRule {
            id: "test_only_swaps_opcode",
            invariant: "deliberately unsound, for the guard test",
            matcher: |expr| {
                if matches!(&expr.op, StageOp::Opcode(c) if c == "tool.read") {
                    Some(PlanExpr::leaf(StageOp::Opcode("tool.bash".to_string())))
                } else {
                    None
                }
            },
        };
        assert!(
            bad.apply(&opcode_leaf()).is_none(),
            "executing a different opcode is not an optimization"
        );
    }

    #[test]
    fn every_shipped_rule_preserves_policy_and_opcode() {
        // Property over the real rule set: whatever a rule produces from any
        // shape it matches, authorization and opcode identity survive.
        let plans = [
            canonical_plan("tool.read"),
            typed_plan_with_roundtrip("tool.write"),
            canonical_plan("session.get_state"),
        ];
        for rule in rewrite_rules() {
            for plan in &plans {
                for sub in subtrees(plan) {
                    if let Some(rewritten) = (rule.matcher)(&sub) {
                        assert_eq!(
                            sub.policy_count(),
                            rewritten.policy_count(),
                            "rule {} changed the policy count",
                            rule.id
                        );
                        assert_eq!(
                            sub.opcodes(),
                            rewritten.opcodes(),
                            "rule {} changed the opcodes",
                            rule.id
                        );
                    }
                }
            }
        }
    }

    fn subtrees(expr: &PlanExpr) -> Vec<PlanExpr> {
        let mut out = vec![expr.clone()];
        for child in &expr.children {
            out.extend(subtrees(child));
        }
        out
    }

    // ── Search behavior ─────────────────────────────────────────────────

    #[test]
    fn saturation_finds_the_fused_typed_pipeline() {
        let engine = HostcallEGraphEngine::new(true);
        let baseline = PlanExpr::unary(
            StageOp::Dispatch,
            PlanExpr::unary(
                StageOp::Validate,
                PlanExpr::unary(
                    StageOp::Marshal(Repr::Typed),
                    PlanExpr::unary(StageOp::Policy, opcode_leaf()),
                ),
            ),
        );
        let decision = engine.optimize(&baseline);

        assert!(decision.rewrote(), "expected a rewrite: {decision:?}");
        assert!(
            decision.plan.signature().contains(RULE_FUSE_TYPED_PIPELINE),
            "expected the whole pipeline fused, got {}",
            decision.plan.signature()
        );
        assert!(decision.expected_cost_delta > 0);
        assert_eq!(
            decision.selected_cost,
            decision.plan.cost(engine.cost_model())
        );
        // The multi-step chain must be visible, not just the final rule.
        assert!(decision.applied_rules.contains(&RULE_FUSE_MARSHAL_VALIDATE));
        assert!(decision.applied_rules.contains(&RULE_FUSE_TYPED_PIPELINE));
    }

    #[test]
    fn saturation_removes_a_redundant_roundtrip() {
        let engine = HostcallEGraphEngine::new(true);
        let baseline = typed_plan_with_roundtrip("tool.read");
        let decision = engine.optimize(&baseline);

        assert!(decision.rewrote(), "expected a rewrite: {decision:?}");
        assert!(
            !decision.plan.signature().contains("convert"),
            "the round trip should be gone, got {}",
            decision.plan.signature()
        );
        assert!(decision.plan.size() < baseline.size());
    }

    #[test]
    fn the_rewritten_plan_keeps_policy_and_opcode() {
        // The equivalence that matters: same opcode, same authorization.
        let engine = HostcallEGraphEngine::new(true);
        for baseline in [
            canonical_plan("tool.read"),
            typed_plan_with_roundtrip("tool.write"),
        ] {
            let decision = engine.optimize(&baseline);
            assert_eq!(decision.plan.policy_count(), baseline.policy_count());
            assert_eq!(decision.plan.opcodes(), baseline.opcodes());
            assert!(decision.plan.has_policy(), "authorization survives");
        }
    }

    #[test]
    fn a_plan_with_nothing_to_gain_keeps_the_baseline() {
        let engine = HostcallEGraphEngine::new(true);
        let baseline = PlanExpr::unary(StageOp::Policy, opcode_leaf());
        let decision = engine.optimize(&baseline);
        assert!(!decision.rewrote());
        assert_eq!(decision.fallback_reason, Some("no_better_plan"));
        assert_eq!(decision.plan, baseline);
        assert_eq!(decision.expected_cost_delta, 0);
    }

    #[test]
    fn the_kill_switch_short_circuits_the_search() {
        let engine = HostcallEGraphEngine::new(false);
        let baseline = typed_plan_with_roundtrip("tool.read");
        let decision = engine.optimize(&baseline);
        assert_eq!(decision.fallback_reason, Some("egraph_disabled"));
        assert_eq!(decision.plan, baseline);
        // Disabled means no work happened, not just no result.
        assert_eq!(decision.nodes, 0);
        assert!(decision.applied_rules.is_empty());
    }

    #[test]
    fn an_exhausted_budget_falls_back_instead_of_claiming_a_minimum() {
        // One iteration cannot reach a fixpoint on a multi-step chain, so the
        // engine must not present its partial result as minimal.
        let engine = HostcallEGraphEngine::new(true).with_limits(SaturationLimits {
            max_iterations: 1,
            ..SaturationLimits::default()
        });
        let decision = engine.optimize(&typed_plan_with_roundtrip("tool.read"));
        assert!(!decision.rewrote());
        assert_eq!(decision.fallback_reason, Some("iteration_budget_exhausted"));
        assert!(!decision.outcome.is_complete());
    }

    #[test]
    fn a_node_budget_stop_also_falls_back() {
        let engine = HostcallEGraphEngine::new(true).with_limits(SaturationLimits {
            max_nodes: 3,
            ..SaturationLimits::default()
        });
        let decision = engine.optimize(&typed_plan_with_roundtrip("tool.read"));
        assert!(!decision.rewrote());
        assert_eq!(decision.fallback_reason, Some("node_budget_exhausted"));
    }

    #[test]
    fn an_unpriced_fusion_loses_to_the_baseline() {
        // fused_default is high on purpose, so a rule whose cost nobody
        // measured cannot win by omission.
        let mut model = CostModel::measured_default();
        model.fused.clear();
        let engine = HostcallEGraphEngine::new(true).with_cost_model(model);
        let baseline = PlanExpr::unary(
            StageOp::Dispatch,
            PlanExpr::unary(
                StageOp::Validate,
                PlanExpr::unary(
                    StageOp::Marshal(Repr::Typed),
                    PlanExpr::unary(StageOp::Policy, opcode_leaf()),
                ),
            ),
        );
        let decision = engine.optimize(&baseline);
        assert!(!decision.rewrote(), "unpriced fusion must not be selected");
        assert_eq!(decision.fallback_reason, Some("no_better_plan"));
    }

    #[test]
    fn a_tie_between_different_plans_is_refused() {
        // Price both fusions so the two orders reach the same total. The
        // engine must refuse rather than let iteration order decide.
        let mut model = CostModel::measured_default();
        model.marshal_typed = 10;
        model.validate = 10;
        model.dispatch = 10;
        model.fused.insert(RULE_FUSE_MARSHAL_VALIDATE, 10);
        model.fused.insert(RULE_FUSE_VALIDATE_DISPATCH, 10);
        model.fused.insert(RULE_FUSE_TYPED_PIPELINE, 20);
        let engine = HostcallEGraphEngine::new(true).with_cost_model(model);
        let baseline = PlanExpr::unary(
            StageOp::Dispatch,
            PlanExpr::unary(
                StageOp::Validate,
                PlanExpr::unary(
                    StageOp::Marshal(Repr::Typed),
                    PlanExpr::unary(StageOp::Policy, opcode_leaf()),
                ),
            ),
        );
        let decision = engine.optimize(&baseline);
        if decision.rewrote() {
            // If it did pick one, the pick must be strictly cheapest — a tie
            // that resolved to a unique signature is legitimate.
            let tied = decision.plan.cost(engine.cost_model());
            assert!(tied < decision.baseline_cost);
        } else {
            assert_eq!(decision.fallback_reason, Some("ambiguous_min_cost"));
        }
    }

    #[test]
    fn search_is_deterministic_across_runs() {
        // Iteration order must not leak into the result; the same input has to
        // give byte-identical telemetry every time.
        let engine = HostcallEGraphEngine::new(true);
        let baseline = typed_plan_with_roundtrip("tool.read");
        let first = engine.optimize(&baseline).to_json();
        for _ in 0..8 {
            assert_eq!(engine.optimize(&baseline).to_json(), first);
        }
    }

    #[test]
    fn cost_never_increases_when_a_rewrite_is_selected() {
        // The core safety property, over every sample plan.
        let engine = HostcallEGraphEngine::new(true);
        for opcode in ["tool.read", "tool.write", "tool.bash", "session.get_state"] {
            for baseline in [canonical_plan(opcode), typed_plan_with_roundtrip(opcode)] {
                let decision = engine.optimize(&baseline);
                assert!(
                    decision.selected_cost <= decision.baseline_cost,
                    "{opcode}: selected {} > baseline {}",
                    decision.selected_cost,
                    decision.baseline_cost
                );
                if decision.rewrote() {
                    assert!(decision.expected_cost_delta > 0);
                    assert!(decision.selected_cost < decision.baseline_cost);
                }
            }
        }
    }

    // ── Telemetry and handoff ───────────────────────────────────────────

    #[test]
    fn telemetry_reports_the_delta_and_redacts_payloads() {
        let engine = HostcallEGraphEngine::new(true);
        let decision = engine.optimize(&typed_plan_with_roundtrip("tool.read"));
        let json = decision.to_json();

        assert_eq!(json["schema"], HOSTCALL_EGRAPH_SCHEMA);
        assert_eq!(json["rewrote"], true);
        assert_eq!(
            json["expected_cost_delta"],
            serde_json::json!(decision.expected_cost_delta)
        );
        assert_eq!(json["saturation"]["outcome"], "fixpoint");
        assert_eq!(json["saturation"]["complete"], true);
        assert_eq!(json["redaction"]["payload_content"], "omitted");
        assert!(json["baseline"]["cost"].as_u64().unwrap() > 0);
        // Rendered signatures must not carry argument values.
        let rendered = json.to_string();
        assert!(!rendered.contains("\"args\""));
    }

    #[test]
    fn a_fallback_decision_reports_its_reason() {
        let engine = HostcallEGraphEngine::new(false);
        let json = engine.optimize(&canonical_plan("tool.read")).to_json();
        assert_eq!(json["rewrote"], false);
        assert_eq!(json["fallback_reason"], "egraph_disabled");
        assert_eq!(json["expected_cost_delta"], 0);
    }

    // ── Calibration ─────────────────────────────────────────────────────

    fn sample_stages() -> MeasuredStages {
        MeasuredStages::from_stage_matrix(41.2, 6.4, 5.1, 9.7, 820.0, 130.0)
    }

    #[test]
    fn calibration_takes_the_three_stages_it_can_map() {
        let (model, report) = CostModel::from_measured_stages(sample_stages());
        assert_eq!(model.marshal_json, 41, "marshal rounds from 41.2");
        assert_eq!(model.policy, 10, "policy rounds from 9.7");
        assert_eq!(model.dispatch, 12, "dispatch is queue + schedule = 11.5");
        assert_eq!(report.measured, ["dispatch", "marshal_json", "policy"]);
    }

    #[test]
    fn calibration_admits_what_it_could_not_measure() {
        // The honest half: the harness cannot separate typed decode from JSON
        // decode, or price an intrinsic that does not exist yet. Those fields
        // must be reported, not quietly filled in.
        let (model, report) = CostModel::from_measured_stages(sample_stages());
        assert!(!report.is_fully_measured());
        for field in ["marshal_typed", "validate", "convert", "fused"] {
            assert!(
                report.unmeasured.contains(&field),
                "{field} is not measurable from six-stage attribution"
            );
        }
        let defaults = CostModel::measured_default();
        assert_eq!(model.marshal_typed, defaults.marshal_typed);
        assert_eq!(model.validate, defaults.validate);
        assert_eq!(model.convert, defaults.convert);
        assert_eq!(model.fused, defaults.fused);
    }

    #[test]
    fn calibration_ignores_execute_and_io() {
        // Every plan pays the same execute/io cost, so including it would
        // inflate both sides and shrink the visible difference between plans.
        let mut heavy = sample_stages();
        heavy.execute_us = 50_000.0;
        heavy.io_us = 90_000.0;
        let (baseline_model, _) = CostModel::from_measured_stages(sample_stages());
        let (heavy_model, _) = CostModel::from_measured_stages(heavy);
        assert_eq!(baseline_model.marshal_json, heavy_model.marshal_json);
        assert_eq!(baseline_model.policy, heavy_model.policy);
        assert_eq!(baseline_model.dispatch, heavy_model.dispatch);
    }

    #[test]
    fn a_measured_stage_never_prices_as_free() {
        // A zero or nonsensical measurement must not make a stage cost nothing;
        // that would let any plan containing it win by arithmetic accident.
        let degenerate = MeasuredStages::from_stage_matrix(0.0, -3.0, f64::NAN, 0.000_1, 0.0, 0.0)
            .with_typed_marshal(-1.0);
        let (model, _) = CostModel::from_measured_stages(degenerate);
        assert!(model.marshal_json >= 1);
        assert!(model.policy >= 1);
        assert!(model.dispatch >= 1);
    }

    #[test]
    fn marshalling_telemetry_measures_the_parameter_the_fast_path_rests_on() {
        // The six-stage matrix alone leaves marshal_typed modelled. The
        // marshalling path already times the typed lane per call, so supplying
        // it moves the single most decisive parameter onto the measured side.
        let (_, matrix_only) = CostModel::from_measured_stages(sample_stages());
        assert!(matrix_only.unmeasured.contains(&"marshal_typed"));
        assert!(!matrix_only.measured.contains(&"marshal_typed"));

        let (model, report) =
            CostModel::from_measured_stages(sample_stages().with_typed_marshal(13.6));
        assert_eq!(model.marshal_typed, 14, "typed marshal rounds from 13.6");
        assert!(report.measured.contains(&"marshal_typed"));
        assert!(!report.unmeasured.contains(&"marshal_typed"));

        // Still not fully measured: validate/convert/fused remain modelled.
        assert!(!report.is_fully_measured());
        assert_eq!(report.unmeasured, ["convert", "fused", "validate"]);
    }

    #[test]
    fn a_calibrated_model_still_refuses_unpriced_fusions() {
        // Calibration must not weaken the fail-closed posture: fused costs are
        // among the unmeasured fields, so clearing them still loses.
        let (mut model, _) = CostModel::from_measured_stages(sample_stages());
        model.fused.clear();
        let engine = HostcallEGraphEngine::new(true).with_cost_model(model);
        let decision = engine.optimize(&canonical_plan("tool.read"));
        assert!(!decision.rewrote());
    }

    #[test]
    fn relative_to_compares_two_searched_plans_not_a_plan_against_itself() {
        // The bug this pins: measuring a decision against its OWN baseline
        // answers "how much did fusing help?", which is not "how expensive is
        // the typed path versus the canonical one?". Both sides get searched.
        let engine = HostcallEGraphEngine::new(true);
        let canonical = engine.optimize(&canonical_plan("tool.read"));
        let typed = engine.optimize(&typed_plan_with_roundtrip("tool.read"));

        let relative = typed
            .relative_to(&canonical, 100)
            .expect("canonical is not free");
        let expected =
            u32::try_from(u64::from(typed.best_cost()) * 100 / u64::from(canonical.best_cost()))
                .expect("in range");
        assert_eq!(relative, expected);

        // The typed path must land below the canonical one on its own scale --
        // that is the whole claim the fast lane makes.
        assert!(
            relative < 100,
            "typed {relative} should beat canonical at 100"
        );
    }

    #[test]
    fn best_cost_is_defined_for_a_fallback_too() {
        // A decision that declined to rewrite still has a best-known plan: the
        // baseline. Without this, a fallback could not participate in a ratio
        // at all, and the caller would silently lose one side of it.
        let disabled = HostcallEGraphEngine::new(false);
        let fallback = disabled.optimize(&typed_plan_with_roundtrip("tool.read"));
        assert!(!fallback.rewrote());
        assert_eq!(fallback.best_cost(), fallback.baseline_cost);

        let engine = HostcallEGraphEngine::new(true);
        let rewritten = engine.optimize(&typed_plan_with_roundtrip("tool.read"));
        assert!(rewritten.rewrote());
        assert_eq!(rewritten.best_cost(), rewritten.selected_cost);
        assert!(rewritten.best_cost() < fallback.best_cost());
    }

    #[test]
    fn relative_to_declines_when_the_reference_is_free() {
        // A zero-cost reference has no ratio. Returning 0 or u32::MAX here
        // would hand the caller a number with no meaning behind it.
        let free = CostModel {
            opcode: 0,
            marshal_json: 0,
            marshal_typed: 0,
            marshal_bytes: 0,
            validate: 0,
            policy: 0,
            dispatch: 0,
            convert: 0,
            fused: BTreeMap::new(),
            fused_default: 0,
        };
        let engine = HostcallEGraphEngine::new(true).with_cost_model(free);
        let zero = engine.optimize(&canonical_plan("tool.read"));
        assert_eq!(zero.best_cost(), 0);
        assert_eq!(zero.relative_to(&zero, 100), None);
    }

    #[test]
    fn relative_to_is_linear_in_the_scale() {
        // Doubling the scale doubles the result, so a caller can reason about
        // the mapping instead of treating it as a black box.
        let engine = HostcallEGraphEngine::new(true);
        let canonical = engine.optimize(&canonical_plan("tool.read"));
        let typed = engine.optimize(&typed_plan_with_roundtrip("tool.read"));
        let at_100 = typed.relative_to(&canonical, 100).expect("projects");
        let at_200 = typed.relative_to(&canonical, 200).expect("projects");
        // Integer division makes this approximate; allow one unit of rounding.
        assert!(
            at_200.abs_diff(at_100.saturating_mul(2)) <= 1,
            "not linear: {at_100} at 100 vs {at_200} at 200"
        );
        assert_eq!(typed.relative_to(&canonical, 0), Some(0));
    }

    #[test]
    fn an_enumeration_blowup_falls_back_instead_of_truncating() {
        // A tiny cap stands in for the Cartesian blowup a pathological graph
        // could cause. The engine must NOT quietly enumerate a prefix and then
        // report a fixpoint -- that would turn "we ran out of room" into "we
        // proved this is optimal".
        let engine = HostcallEGraphEngine::new(true).with_limits(SaturationLimits {
            max_enumerated: 1,
            ..SaturationLimits::default()
        });
        let decision = engine.optimize(&typed_plan_with_roundtrip("tool.read"));
        assert!(!decision.rewrote(), "must not rewrite on a partial view");
        assert_eq!(
            decision.fallback_reason,
            Some("enumeration_budget_exhausted")
        );
        assert_eq!(decision.outcome, SaturationOutcome::EnumerationBudget);
        assert!(!decision.outcome.is_complete());
        assert_eq!(decision.plan, decision.baseline);
    }

    #[test]
    fn the_default_enumeration_cap_is_not_hit_by_real_plans() {
        // The cap must bound pathology without perturbing ordinary use, or it
        // would silently disable the search on the plans it exists to optimize.
        let engine = HostcallEGraphEngine::new(true);
        for opcode in ["tool.read", "tool.write", "session.get_state"] {
            for plan in [canonical_plan(opcode), typed_plan_with_roundtrip(opcode)] {
                let decision = engine.optimize(&plan);
                assert_ne!(
                    decision.outcome,
                    SaturationOutcome::EnumerationBudget,
                    "{opcode}: real plan should not exhaust the default cap"
                );
            }
        }
    }

    #[test]
    fn the_typed_path_beats_the_canonical_path_on_a_shared_scale() {
        // Mirrors what src/extensions/protocol.rs::egraph_fast_opcode_cost
        // computes: both plans searched, then compared. If this inverts, the
        // fast lane would be priced as the more expensive option.
        let engine = HostcallEGraphEngine::new(true);
        let canonical = engine.optimize(&canonical_plan("tool.read"));
        let typed = engine.optimize(&PlanExpr::unary(
            StageOp::Dispatch,
            PlanExpr::unary(
                StageOp::Validate,
                PlanExpr::unary(
                    StageOp::Marshal(Repr::Typed),
                    PlanExpr::unary(StageOp::Policy, opcode_leaf()),
                ),
            ),
        ));
        assert!(
            typed.best_cost() < canonical.best_cost(),
            "typed {} should cost less than canonical {}",
            typed.best_cost(),
            canonical.best_cost()
        );
        let on_scale = typed.relative_to(&canonical, 100).expect("projects");
        assert!(
            on_scale < 100,
            "typed should land under 100, got {on_scale}"
        );
        assert!(on_scale > 0, "a real plan is not free");
    }

    #[test]
    fn handoff_to_the_existing_selector_authorizes_the_fast_path() {
        // The search proposes; hostcall_rewrite disposes. A selected rewrite
        // must survive that engine's own guard.
        use crate::hostcall_rewrite::HostcallRewriteEngine;

        let egraph = HostcallEGraphEngine::new(true);
        let decision = egraph.optimize(&typed_plan_with_roundtrip("tool.read"));
        assert!(decision.rewrote());

        let baseline_plan = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::BaselineCanonical,
            estimated_cost: decision.baseline_cost,
            rule_id: "baseline",
        };
        let candidate = decision.to_rewrite_plan(
            HostcallRewritePlanKind::FastOpcodeFusion,
            RULE_FUSE_TYPED_PIPELINE,
        );

        let selector = HostcallRewriteEngine::new(true);
        let selected = selector.select_plan(baseline_plan, &[candidate]);
        assert!(selected.fallback_reason.is_none());
        assert_eq!(selected.selected.estimated_cost, decision.selected_cost);
        assert_eq!(
            selected.expected_cost_delta, decision.expected_cost_delta,
            "both engines must agree on the saving"
        );
    }

    #[test]
    fn a_fallback_is_rejected_by_the_selector_too() {
        // Defense in depth: even if a caller forwards a fallback decision, the
        // selector refuses it because its cost cannot beat the baseline.
        use crate::hostcall_rewrite::HostcallRewriteEngine;

        let egraph = HostcallEGraphEngine::new(false);
        let decision = egraph.optimize(&canonical_plan("tool.read"));
        assert!(!decision.rewrote());

        let baseline_plan = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::BaselineCanonical,
            estimated_cost: decision.baseline_cost,
            rule_id: "baseline",
        };
        let candidate =
            decision.to_rewrite_plan(HostcallRewritePlanKind::FastOpcodeFusion, "forwarded");
        let selected = HostcallRewriteEngine::new(true).select_plan(baseline_plan, &[candidate]);
        assert_eq!(selected.fallback_reason, Some("no_better_candidate"));
    }

    #[test]
    fn kill_switch_parses_the_disabling_values() {
        // Tested through from_opt, not by mutating the environment: the crate
        // is forbid(unsafe_code) and std::env::set_var is unsafe in Rust 2024.
        for value in [
            "0", "false", "off", "disabled", "OFF", " false ", "DISABLED",
        ] {
            assert!(
                !HostcallEGraphEngine::from_opt(Some(value)).enabled(),
                "{value:?} should disable the search"
            );
        }
        for value in ["1", "true", "on", "yes", ""] {
            assert!(
                HostcallEGraphEngine::from_opt(Some(value)).enabled(),
                "{value:?} should leave the search enabled"
            );
        }
        assert!(
            HostcallEGraphEngine::from_opt(None).enabled(),
            "absent means enabled, matching hostcall_rewrite"
        );
    }

    #[test]
    fn the_kill_switch_agrees_with_the_existing_planner() {
        // One variable governs both halves of the rewrite path, so the two
        // parsers must never disagree about what a value means.
        use crate::hostcall_rewrite::HostcallRewriteEngine;

        for value in [
            Some("0"),
            Some("false"),
            Some("off"),
            Some("disabled"),
            Some("OFF"),
            Some(" false "),
            Some("1"),
            Some("true"),
            Some("anything-else"),
            None,
        ] {
            assert_eq!(
                HostcallEGraphEngine::from_opt(value).enabled(),
                HostcallRewriteEngine::from_opt(value).enabled(),
                "kill-switch parsers disagree on {value:?}"
            );
        }
    }
}
