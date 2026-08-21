#!/usr/bin/env bash
# scripts/e2e/run_skills_managed.sh — Focused e2e lane for learn +
# manage_skill (bd-cv653.4.2).
#
# Hermetic: runs the skills_managed:: unit suite (lint gate, CRUD cycle,
# unmanaged-content refusal matrix) plus the skills_managed integration
# target (promote→discover with managed provenance, user-shadows-managed
# collision diagnostic, refusal through the tool surface, invalid-promote
# lesson-kept warning, bank gating, CRUD, audit ledger). Unique
# pid-suffixed skill names; no env mutation; no network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/skills-managed/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-skills-managed-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[skills-managed] Running skills_managed:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib skills_managed:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[skills-managed] Running skills_managed integration target (correlation: $CORRELATION_ID)"
cargo test --test skills_managed -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[skills-managed] PASS (artifacts: $ARTIFACT_DIR)"
