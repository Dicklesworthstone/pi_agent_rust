# Command Recipes

## 1) Session Bootstrap

```bash
export CARGO_TARGET_DIR="/data/tmp/pi_agent_rust/${USER:-agent}"
export TMPDIR="/data/tmp/pi_agent_rust/${USER:-agent}/tmp"
mkdir -p "$TMPDIR"
```

## 2) Fast Recon

```bash
git status --short
rg -n "TODO|FIXME|install|uninstall|skill|checksum|sigstore|completion|provider|session|extension" \
  install.sh uninstall.sh README.md tests/installer_regression.sh src/
```

## 3) Skill Integrity and Sync Guard

```bash
bash scripts/skill-smoke.sh
```

## 4) Installer and Shell Safety Gates

```bash
bash -n install.sh uninstall.sh tests/installer_regression.sh
shellcheck -x install.sh uninstall.sh tests/installer_regression.sh
bash tests/installer_regression.sh
```

## 5) Rust Quality Gates

```bash
# Preferred in multi-agent environments
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings

# Formatting can run locally
cargo fmt --check
```

## 6) Targeted Test Slices

```bash
# Tool behavior
cargo test tools

# Provider streaming/protocol: VCR playback for every provider (tests/provider_streaming.rs
# and tests/provider_streaming/*.rs). A bare `cargo test provider_streaming` is a
# name filter, and no test path contains that string, so it runs nothing.
cargo test --test provider_streaming
# Anthropic end-to-end scenarios (tests/e2e_provider_streaming.rs)
cargo test --test e2e_provider_streaming

# Session persistence/index
cargo test session

# Extension runtime/policy
cargo test extension

# RPC surface (tests/e2e_rpc.rs target)
cargo test --test e2e_rpc

# Broader safety net after targeted slices
cargo test conformance
```

## 7) Focused Installer Branch Debugging

```bash
# Help/flag visibility
bash install.sh --help

# Explicit custom artifact path (replace values for real run)
bash install.sh --yes --offline --version v0.0.0 \
  --artifact-url "file:///tmp/pi-artifact" \
  --checksum "<sha256>" \
  --no-completions --no-agent-skills

# Uninstall smoke
bash uninstall.sh --yes --no-gum
```

## 8) Status and Safety Tracing

```bash
# Installer status flow
rg -n "AGENT_SKILL_STATUS|CHECKSUM_STATUS|SIGSTORE_STATUS|COMPLETIONS_STATUS" install.sh

# Skill install safety + replacement
rg -n "install_skill_to_destination|is_installer_managed_skill_file|is_expected_skill_destination" install.sh

# Uninstall safety guards
rg -n "remove_installed_skills|is_managed_skill_file|is_expected_skill_directory|PIAR_AGENT_SKILL" uninstall.sh
```

## 9) Docs Drift Checks

```bash
rg -n "no-agent-skills|completions|checksum|sigstore|artifact-url|skill" README.md install.sh uninstall.sh
```

For symptom-driven step-by-step root-cause flows, use `DEBUGGING-PLAYBOOKS.md`.
