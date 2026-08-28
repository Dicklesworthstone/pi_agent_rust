#!/usr/bin/env bash
# scripts/perf/preflight_dsr_recipe.sh
#
# Preflight check for the DSR perf recipe (bd-ri-phase1-recipe-audit).
# Verifies every hidden contract documented in docs/perf-budgets-recipe.md
# before launching an expensive DSR run that consumes hours of RCH
# worker time and produces artifacts that are only valid if every
# contract is honored.
#
# Exit 0 = ready to run DSR.
# Exit 1 = at least one contract violated; see runpack JSON for
#          which one.
#
# Usage:
#   bash scripts/perf/preflight_dsr_recipe.sh \
#     --dsr /Users/jemanuel/projects/doodlestein_self_releaser/dsr \
#     --work-dir /Users/jemanuel/projects/pi_agent_rust \
#     --out docs/evidence/ri-phase1-recipe-audit-runpack.json
#
# Environment overrides:
#   PREFILLIGHT_DSR_DEFAULT   default dsr binary path
#   PREFILLIGHT_WORKDIR_DEFAULT default work dir

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DSR="${PREFILLIGHT_DSR_DEFAULT:-/Users/jemanuel/projects/doodlestein_self_releaser/dsr}"
WORKDIR="${PREFILLIGHT_WORKDIR_DEFAULT:-$PROJECT_ROOT}"
OUT=""

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \?//'
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dsr)       DSR="$2"; shift 2 ;;
    --work-dir)  WORKDIR="$2"; shift 2 ;;
    --out)       OUT="$2"; shift 2 ;;
    -h|--help)   usage ;;
    *)           echo "unknown arg: $1" >&2; usage ;;
  esac
done

OUT="${OUT:-$WORKDIR/docs/evidence/ri-phase1-recipe-audit-runpack.json}"

# ─── Result accumulator ─────────────────────────────────────────────
findings=()
ok_count=0
fail_count=0
warn_count=0

record() {
  local status="$1"; shift
  local contract="$1"; shift
  local detail="$1"; shift || true
  local json
  json=$(jq -nc \
    --arg contract "$contract" \
    --arg status "$status" \
    --arg detail "$detail" \
    '{contract:$contract, status:$status, detail:$detail}')
  findings+=("$json")
  case "$status" in
    pass) ok_count=$((ok_count+1)) ;;
    fail) fail_count=$((fail_count+1)) ;;
    warn) warn_count=$((warn_count+1)) ;;
  esac
}

# ─── 1. DSR binary exists and is executable ─────────────────────────
if [[ -x "$DSR" ]]; then
  record pass "DSR_BINARY_PRESENT" "$DSR"
else
  record fail "DSR_BINARY_PRESENT" \
    "dsr binary not found at $DSR; set --dsr to override"
fi

# ─── 2. DSR version sane ───────────────────────────────────────────
if [[ -x "$DSR" ]]; then
  if DSR_VERSION=$("$DSR" --version 2>/dev/null | head -1); then
    record pass "DSR_VERSION_KNOWN" "$DSR_VERSION"
  else
    record fail "DSR_VERSION_KNOWN" "dsr --version failed"
  fi
fi

# ─── 3. Work dir is the pi_agent_rust repo ─────────────────────────
if [[ -f "$WORKDIR/Cargo.toml" ]] && \
   grep -q '^name = "pi_agent_rust"' "$WORKDIR/Cargo.toml" 2>/dev/null; then
  record pass "WORKDIR_IS_PI_AGENT_RUST" "$WORKDIR"
else
  record fail "WORKDIR_IS_PI_AGENT_RUST" \
    "$WORKDIR does not look like pi_agent_rust (no Cargo.toml or wrong name)"
fi

# ─── 4. AGENTS.md forbids direct cargo / RCH; DSR is the path ─────
if [[ -f "$WORKDIR/AGENTS.md" ]] && \
   grep -q "dsr quality --tool pi_agent_rust" "$WORKDIR/AGENTS.md"; then
  record pass "AGENTS_DSR_GOVERNANCE_PRESENT" "AGENTS.md mandates dsr quality"
else
  record fail "AGENTS_DSR_GOVERNANCE_PRESENT" \
    "AGENTS.md does not mention dsr quality --tool pi_agent_rust"
fi

# ─── 5. DSR repos.yaml has a pi_agent_rust entry with checks ──────
REPOS_YAML="${HOME}/.config/dsr/repos.yaml"
if [[ -f "$REPOS_YAML" ]] && \
   grep -q "pi_agent_rust" "$REPOS_YAML"; then
  record pass "DSR_REPOS_YAML_HAS_PI_AGENT_RUST" "$REPOS_YAML"
else
  record fail "DSR_REPOS_YAML_HAS_PI_AGENT_RUST" \
    "$REPOS_YAML missing pi_agent_rust entry"
fi

if [[ -x "$DSR" ]]; then
  set +e
  DSR_DRY=$("$DSR" quality --tool pi_agent_rust --dry-run \
               --work-dir "$WORKDIR" 2>&1)
  DSR_DRY_RC=$?
  set -e
  PLANNED_LINES=$(echo "$DSR_DRY" | grep -c "Planned, NOT executed" || true)
  if [[ "${PLANNED_LINES:-0}" -ge 6 ]]; then
    record pass "DSR_DRY_RUN_PLANS_6_CHECKS" \
      "dsr plans $PLANNED_LINES checks"
  else
    record fail "DSR_DRY_RUN_PLANS_6_CHECKS" \
      "dsr plans $PLANNED_LINES checks, expected >=6 (dsr --dry-run rc=$DSR_DRY_RC)"
  fi
fi

# ─── 7. The /tmp/pi-agent-rust-dsr anti-pattern is documented ─────
# (this is a WARN, not a fail, because the operator may have patched
# the recipe; it just means we have to verify the resulting
# CARGO_TARGET_DIR is on the external NVMe)
if grep -q "/tmp/pi-agent-rust-dsr" "$REPOS_YAML" 2>/dev/null; then
  record warn "DSR_TMP_TARGET_DIR_USED" \
    "repos.yaml hardcodes CARGO_TARGET_DIR=/tmp/pi-agent-rust-dsr; this is the AGENTS.md anti-pattern. Either set RCH_TARGET_BASE and patch, or accept and clean up post-run with sbh check."
fi

# ─── 8. rch is on PATH and healthy ────────────────────────────────
if command -v rch >/dev/null 2>&1; then
  record pass "RCH_ON_PATH" "$(command -v rch)"
  if RCH_DIAG=$(rch diagnose 2>&1); then
    if echo "$RCH_DIAG" | grep -qiE "healthy|ready|admitted"; then
      record pass "RCH_FLEET_HEALTHY" "rch diagnose indicates at least one healthy worker"
    else
      record warn "RCH_FLEET_HEALTHY" \
        "rch diagnose did not return a clear healthy signal; DSR may fall back to local cargo which produces invalid evidence"
    fi
  else
    record warn "RCH_DIAGNOSE_FAILED" \
      "rch diagnose failed; DSR may fall back to local cargo"
  fi
else
  record fail "RCH_ON_PATH" \
    "rch not on PATH; DSR cannot offload compilation; AGENTS.md requires RCH"
fi

# ─── 9. .rchignore exists and is non-trivial ──────────────────────
RCHIGNORE="$WORKDIR/.rchignore"
if [[ -f "$RCHIGNORE" ]] && [[ $(wc -l < "$RCHIGNORE") -ge 20 ]]; then
  record pass "RCHIGNORE_PRESENT" "$RCHIGNORE ($(wc -l < "$RCHIGNORE") lines)"
else
  record fail "RCHIGNORE_PRESENT" \
    "$RCHIGNORE missing or too small; the recipe assumes this file"
fi

# ─── 10. preflight_budget_inputs.py exists and runs ───────────────
PFB="$WORKDIR/scripts/perf/preflight_budget_inputs.py"
if [[ -f "$PFB" ]]; then
  if (cd "$WORKDIR" && python3 "$PFB" --help >/dev/null 2>&1); then
    record pass "PREFLIGHT_BUDGET_INPUTS_PRESENT" "$PFB"
  else
    record fail "PREFLIGHT_BUDGET_INPUTS_PRESENT" \
      "$PFB exists but is not runnable"
  fi
else
  record fail "PREFLIGHT_BUDGET_INPUTS_PRESENT" \
    "$PFB missing; the DSR recipe assumes it"
fi

# ─── 11. check_module_reachability.py exists ─────────────────────
CMR="$WORKDIR/scripts/check_module_reachability.py"
if [[ -f "$CMR" ]]; then
  record pass "CHECK_MODULE_REACHABILITY_PRESENT" "$CMR"
else
  record fail "CHECK_MODULE_REACHABILITY_PRESENT" \
    "$CMR missing; the DSR recipe's check #6 will fail"
fi

# ─── 12. tests/installer_regression.sh exists ────────────────────
TIR="$WORKDIR/tests/installer_regression.sh"
if [[ -f "$TIR" ]]; then
  record pass "INSTALLER_REGRESSION_PRESENT" "$TIR"
else
  record fail "INSTALLER_REGRESSION_PRESENT" \
    "$TIR missing; DSR check #5 will fail"
fi

# ─── 13. evidence-contract-schema.json has the perf schemas ─────
ECS="$WORKDIR/docs/evidence-contract-schema.json"
SCHEMAS_NEEDED=(
  "pi.perf.budget_summary.v1"
  "pi.perf.budget_preflight.v1"
  "pi.perf.evidence_cache.v1"
  "pi.perf.phase1_matrix_validation.v1"
  "pi.perf.host_topology_fingerprint.v1"
)
missing_schemas=()
if [[ -f "$ECS" ]]; then
  for s in "${SCHEMAS_NEEDED[@]}"; do
    if ! grep -q "\"$s\"" "$ECS" 2>/dev/null; then
      missing_schemas+=("$s")
    fi
  done
  if [[ ${#missing_schemas[@]} -eq 0 ]]; then
    record pass "EVIDENCE_SCHEMAS_PRESENT" \
      "all ${#SCHEMAS_NEEDED[@]} required schemas in $ECS"
  else
    record fail "EVIDENCE_SCHEMAS_PRESENT" \
      "missing schemas in $ECS: ${missing_schemas[*]}"
  fi
else
  record fail "EVIDENCE_SCHEMAS_PRESENT" \
    "$ECS missing"
fi

# ─── 14. closeout-evidence-registry exists and is fresh ──────────
CER="$WORKDIR/docs/contracts/closeout-evidence-registry.json"
if [[ -f "$CER" ]]; then
  record pass "CLOSEOUT_EVIDENCE_REGISTRY_PRESENT" "$CER"
else
  record warn "CLOSEOUT_EVIDENCE_REGISTRY_PRESENT" \
    "$CER missing; the budget_summary may refuse to flip claim_readiness"
fi

# ─── 15. budget_summary.json exists and is parseable ────────────
BS="$WORKDIR/tests/perf/reports/budget_summary.json"
if [[ -f "$BS" ]]; then
  if BS_STATUS=$(jq -r '.claim_readiness.status // "missing"' "$BS" 2>/dev/null); then
    record pass "BUDGET_SUMMARY_PARSEABLE" \
      "claim_readiness.status=$BS_STATUS"
  else
    record fail "BUDGET_SUMMARY_PARSEABLE" \
      "$BS is not valid JSON"
  fi
else
  record warn "BUDGET_SUMMARY_PRESENT" \
    "$BS missing; the recipe cannot update it (this is expected for a fresh checkout)"
fi

# ─── Write runpack ────────────────────────────────────────────────
mkdir -p "$(dirname "$OUT")"
VERDICT="ready"
if [[ $fail_count -gt 0 ]]; then
  VERDICT="not_ready"
elif [[ $warn_count -gt 0 ]]; then
  VERDICT="ready_with_warnings"
fi

# Write findings as a JSON array
findings_json="["
for i in "${!findings[@]}"; do
  if [[ $i -gt 0 ]]; then findings_json+=","; fi
  findings_json+="${findings[$i]}"
done
findings_json+="]"

# Run diagnose if available for richer provenance
RCH_DIAGNOSE_OUT="null"
if command -v rch >/dev/null 2>&1; then
  RCH_DIAGNOSE_OUT=$(rch diagnose 2>&1 | head -50 || echo "rch diagnose failed")
  RCH_DIAGNOSE_OUT=$(jq -nc --arg s "$RCH_DIAGNOSE_OUT" '$s')
fi

GIT_COMMIT=$(cd "$WORKDIR" && git rev-parse HEAD 2>/dev/null || echo "unknown")
GIT_DIRTY=$(cd "$WORKDIR" && git status --porcelain 2>/dev/null \
              | head -1 || echo "")
if [[ -n "$GIT_DIRTY" ]]; then GIT_DIRTY=true; else GIT_DIRTY=false; fi

TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)

jq -n \
  --arg schema "pi.evidence.ri_phase1_recipe_audit_runpack.v1" \
  --arg generated_at "$TIMESTAMP" \
  --arg workdir "$WORKDIR" \
  --arg dsr "$DSR" \
  --argjson ok_count "$ok_count" \
  --argjson fail_count "$fail_count" \
  --argjson warn_count "$warn_count" \
  --arg verdict "$VERDICT" \
  --arg git_commit "$GIT_COMMIT" \
  --argjson git_dirty "$GIT_DIRTY" \
  --argjson findings "$findings_json" \
  --argjson rch_diagnose "$RCH_DIAGNOSE_OUT" \
  '{
    schema: $schema,
    generated_at: $generated_at,
    workdir: $workdir,
    dsr: $dsr,
    git_commit: $git_commit,
    git_dirty: $git_dirty,
    ok_count: $ok_count,
    fail_count: $fail_count,
    warn_count: $warn_count,
    verdict: $verdict,
    findings: $findings,
    rch_diagnose_head: $rch_diagnose
  }' > "$OUT"

# ─── Summary ──────────────────────────────────────────────────────
echo "preflight verdict: $VERDICT"
echo "  pass: $ok_count"
echo "  fail: $fail_count"
echo "  warn: $warn_count"
echo "  runpack: $OUT"

if [[ $fail_count -gt 0 ]]; then
  echo
  echo "FAIL details:"
  for f in "${findings[@]}"; do
    if echo "$f" | jq -e '.status == "fail"' >/dev/null; then
      echo "  $(echo "$f" | jq -r '.contract'): $(echo "$f" | jq -r '.detail')"
    fi
  done
  exit 1
fi
exit 0
