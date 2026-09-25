# Debugging Playbooks

Use this file when the symptom is known but the root cause is unclear.
Each playbook is symptom-first and ends with a concrete fix verification checklist.

## Error Signature Map

| Error Signature (example) | Likely Root Area | First Command |
|---|---|---|
| `Release checksum verification failed; aborting install` | Checksum source/manifest parsing in installer | `rg -n "checksum|SHA256SUMS|CHECKSUM_STATUS" install.sh tests/installer_regression.sh` |
| `Release signature verification failed; aborting install` | Sigstore/cosign path or bundle handling | `rg -n "sigstore|cosign|SIGSTORE_STATUS" install.sh tests/installer_regression.sh` |
| `Custom artifact download failed; cannot fall back to source` | Synthetic custom artifact flow | `rg -n "custom-artifact|artifact-url|fall back to source" install.sh tests/installer_regression.sh` |
| `Skills:    partial (...)` | Mixed skill-install outcome logic | `rg -n "install_agent_skills|AGENT_SKILL_STATUS|failed_writes|skipped_custom" install.sh` |
| `Skipping unexpected skill directory path:` | Uninstall path guard triggered | `rg -n "is_expected_skill_directory|remove_installed_skills" uninstall.sh` |
| Streaming/tool-call mismatch in provider tests | Provider streaming/event normalization | `cargo test --test provider_streaming -- --nocapture` |
| Session replay/index drift | Session persistence/index metadata logic | `cargo test session -- --nocapture` |
| Extension hostcall/capability denial mismatch | Extension policy + QuickJS bridge | `cargo test extension -- --nocapture` |

## Playbook 1: Provider Streaming / Tool-Call Regressions

### Symptoms
- Streaming stalls, truncates, or emits malformed deltas.
- Tool-call events differ between provider backends.
- Provider streaming tests fail after provider or parser edits.

### First 3 Commands

```bash
cargo test --test provider_streaming -- --nocapture
rg -n "stream|tool|delta|event|SSE|responses|completions" src/providers src/provider.rs src/sse.rs
cargo test conformance
```

`--test provider_streaming` replays the VCR cassettes for every provider (one module per provider:
`anthropic::`, `openai::`, `gemini::` ...). `--test e2e_provider_streaming` adds the Anthropic
end-to-end scenarios. Under rch, set `PI_PROVIDER_REPLAY_GIT_COMMIT="$(git rev-parse HEAD)"`, because
rch omits repository metadata.

### Minimal Repro Template

```bash
# Replace with the narrowest failing test path from the output, e.g. anthropic::<case>.
cargo test --test provider_streaming <failing_case> -- --nocapture
```

### Narrow the Change Surface

```bash
rg -n "impl Provider|stream|tool" src/providers/*.rs src/providers/mod.rs src/provider.rs
rg -n "parse|event|data:" src/sse.rs
```

### Fix Verification Checklist

- [ ] Failing provider streaming case reproduces before fix.
- [ ] Targeted provider test passes after fix.
- [ ] No regression in `cargo test conformance`.
- [ ] Error/status messaging remains explicit and unchanged unless intentional.

## Playbook 2: Session Persistence / Index Drift

### Symptoms
- Session replay/history differs unexpectedly between runs.
- Session index metadata mismatches stored entries.
- Save/open path behavior regresses after session changes.

### First 3 Commands

```bash
cargo test session -- --nocapture
rg -n "Session|save|open|index|jsonl|sqlite" src/session.rs src/session_index.rs src/session_test.rs
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with the full failing test path printed in the output
# (lib tests look like session::tests::<case>).
cargo test <failing_test_path> -- --nocapture
```

### Narrow the Change Surface

```bash
rg -n "append|save|open|diagnostic|branch|metadata" src/session.rs src/session_index.rs
```

### Fix Verification Checklist

- [ ] One deterministic session test reproduces before fix.
- [ ] Session test slice passes after fix.
- [ ] Conformance remains green for touched behavior.
- [ ] No undocumented format/semantic drift introduced.

## Playbook 3: Extension Runtime / Policy Regressions

### Symptoms
- Hostcalls denied/allowed unexpectedly.
- Capability policies behave inconsistently by profile.
- QuickJS runtime behavior diverges from expected policy enforcement.

### First 3 Commands

```bash
cargo test extension -- --nocapture
rg -n "extension|policy|hostcall|capability|quickjs|security|deny|allow" src/extensions.rs src/extensions_js.rs tests/
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with the full failing test path printed in the output
# (the lib module is `extensions`, so `extension::<case>` matches nothing).
cargo test <failing_test_path> -- --nocapture
```

### Narrow the Change Surface

```bash
rg -n "allow|deny|policy|capability|hostcall" src/extensions.rs src/extensions_js.rs
```

### Fix Verification Checklist

- [ ] Failing extension test reproduces before fix.
- [ ] Targeted extension slice passes after fix.
- [ ] Policy semantics are still least-privilege and explicit.
- [ ] Broader conformance remains stable.

## Playbook 4: Installer / Uninstaller / Skill Installation Failures

### Symptoms
- Installer summary status is wrong, vague, or contradictory.
- Existing custom skill directories are modified unexpectedly.
- Uninstall removes unintended paths.
- Checksum/signature/completion branches regress.

### First 3 Commands

```bash
bash tests/installer_regression.sh
rg -n "AGENT_SKILL_STATUS|CHECKSUM_STATUS|SIGSTORE_STATUS|COMPLETIONS_STATUS|install_skill_to_destination" install.sh
rg -n "remove_installed_skills|is_expected_skill_directory|is_managed_skill_file|PIAR_AGENT_SKILL" uninstall.sh
```

### Minimal Repro Template

```bash
# Run a single installer regression case by editing tests/installer_regression.sh
# to isolate the failing test, then:
bash tests/installer_regression.sh

# Skill integrity + inline sync guard:
bash scripts/skill-smoke.sh
```

### Narrow the Change Surface

```bash
rg -n "install_skill_to_destination|install_agent_skills|write_state|print_summary" install.sh
rg -n "remove_installed_skills|is_expected_skill_directory|is_managed_skill_file" uninstall.sh
```

### Fix Verification Checklist

- [ ] Failing installer regression case reproduces before fix.
- [ ] `bash tests/installer_regression.sh` passes after fix.
- [ ] `bash scripts/skill-smoke.sh` passes after fix.
- [ ] Custom-skill preservation and managed-only deletion behavior remains intact.

## Playbook 5: CLI/TUI vs RPC Divergence

### Symptoms
- Interactive mode works while RPC/stdin mode fails (or inverse).
- Event ordering/shape differs by surface.

### First 3 Commands

```bash
cargo test --test e2e_rpc -- --nocapture
rg -n "interactive|rpc|stdin|event|session" src/main.rs src/interactive.rs src/rpc.rs
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with specific failing RPC test from output.
cargo test --test e2e_rpc <failing_case> -- --nocapture
```

### Fix Verification Checklist

- [ ] Failing RPC case reproduces before fix.
- [ ] Targeted RPC test passes after fix.
- [ ] Event behavior remains consistent across surfaces.
- [ ] Broader conformance still passes.

## Standard Escalation Path

Use this only after targeted repro + narrow slice:

```bash
# 1) targeted failing slice first
cargo test <targeted-slice> -- --nocapture

# 2) local invariants for changed code
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check

# 3) broader behavior signal
cargo test conformance
```

## Root-Cause Confirmation Checklist

- [ ] Reproduced the failure with a deterministic command.
- [ ] Identified one minimal change that explains the symptom.
- [ ] Added or updated regression coverage for the fixed path.
- [ ] Verified failure before fix and pass after fix on targeted slice.
- [ ] Ran broader safety gates for touched surface.
- [ ] Updated docs/skill guidance if user-visible behavior changed.
