# Longtail Provider Evidence (bd-3uqg.11.10.7)

Quick-win longtail provider mappings, explicit deferrals with user impact, and
links to passing test evidence so decisions are auditable and reproducible.

Generated: 2026-02-13

> Historical snapshot: provider lists, counts, deferrals, and commands below
> describe the 2026-02-13 audit and are retained for provenance. They are not
> current release authority. Current registry and dispatch truth live in
> `src/provider_metadata.rs`, `src/providers/mod.rs`, and their executable tests.

Updated 2026-08-06: the former `github-models` quick-win route was removed
after GitHub retired the GitHub Models service on 2026-07-30. The separate
`github-copilot` native provider remains supported.

## Quick-Win Providers (Implemented + Tested)

All quick-win providers route through the OpenAI-compatible adapter
(`openai-completions`) with provider-specific base URL and auth env keys
defined in `src/provider_metadata.rs`.

### Copy-Paste Configuration

Each provider requires only an API key environment variable. No `models.json`
entry is needed for basic usage.

```bash
# Mistral
export MISTRAL_API_KEY="your-key"
pi --provider mistral --model mistral-large-latest -p "Say hello"

# DeepInfra
export DEEPINFRA_API_KEY="your-key"
pi --provider deepinfra --model meta-llama/Meta-Llama-3.1-70B-Instruct -p "Say hello"

# Together AI
export TOGETHER_API_KEY="your-key"
pi --provider togetherai --model meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo -p "Say hello"

# NVIDIA NIM
export NVIDIA_API_KEY="your-key"
pi --provider nvidia --model meta/llama-3.1-70b-instruct -p "Say hello"

# Hugging Face Inference
export HF_TOKEN="your-key"
pi --provider huggingface --model meta-llama/Meta-Llama-3.1-70B-Instruct -p "Say hello"

# StackIT
export STACKIT_API_KEY="your-key"
pi --provider stackit --model stackit-chat -p "Say hello"

# SiliconFlow
export SILICONFLOW_API_KEY="your-key"
pi --provider siliconflow --model Qwen/Qwen2.5-72B-Instruct -p "Say hello"
```

### Representative Test Coverage

| Provider | Auth Env Key | Contract Tests | Conformance Tests | E2E Tests |
|---|---|---|---|---|
| stackit | `STACKIT_API_KEY` | 8 (provider_native_contract.rs) | 7 (provider_native_verify.rs) | E2E_FAMILIES (e2e_provider_scenarios.rs) |
| mistral | `MISTRAL_API_KEY` | 8 | 7 | E2E_FAMILIES |
| deepinfra | `DEEPINFRA_API_KEY` | 8 | 7 | E2E_FAMILIES |
| togetherai | `TOGETHER_API_KEY` | 8 | 7 | E2E_FAMILIES |
| nvidia | `NVIDIA_API_KEY` | 8 | 7 | E2E_FAMILIES |
| huggingface | `HF_TOKEN` | 8 | 7 | E2E_FAMILIES |
| ollama-cloud | `OLLAMA_API_KEY` | 8 | 7 | E2E_FAMILIES |

### Evidence Artifacts

- **Contract tests**: `tests/provider_native_contract.rs::longtail_contract::*`
  - 56 per-provider tests (8 per provider x 7 providers)
  - 5 metadata consistency tests in `longtail_provider_metadata`
  - Run: `cargo test --test provider_native_contract -- longtail_contract`
- **Conformance tests**: `tests/provider_native_verify.rs::*_conformance`
  - 49 VCR-based conformance tests (7 scenarios x 7 providers)
  - Run: `cargo test --test provider_native_verify -- stackit_conformance mistral_conformance deepinfra_conformance togetherai_conformance nvidia_conformance huggingface_conformance ollama_cloud_conformance`
- **E2E tests**: `tests/e2e_provider_scenarios.rs`
  - 16 families in E2E_FAMILIES (includes all 7 longtail providers)
  - 13 wave presets in `e2e_openai_compatible_wave_presets`
  - Run: `cargo test --test e2e_provider_scenarios`
- **Failure taxonomy**: `tests/provider_native_contract.rs::failure_taxonomy`
  - 7 tests validating error hint coverage for all 12 providers
  - Run: `cargo test --test provider_native_contract -- failure_taxonomy`
- **Registry guardrails**: `tests/provider_registry_guardrails.rs`
  - Drift-prevention tests ensuring upstream deltas are triageable
  - Run: `cargo test --test provider_registry_guardrails`

### Failure Taxonomy per Provider

Each quick-win provider's error hints are validated through `src/error.rs::provider_hints()`:

| Failure Category | Error Pattern | Remediation |
|---|---|---|
| Missing API key | "missing api key" | Set env var (provider-specific) |
| Auth failure (401) | "401", "unauthorized" | Verify API key, check org permissions |
| Forbidden (403) | "403", "forbidden" | Check model access and account permissions |
| Rate limit (429) | "429", "too many requests" | Wait and retry, reduce request rate |
| Quota exceeded | "insufficient_quota" | Verify billing/credits |
| Overloaded (529) | "529", "overloaded" | Retry later |
| Timeout | "request timed out" | Retry, check network connectivity |

## Additional OpenAI-Compatible Providers (Metadata-Only)

These providers have metadata entries in `src/provider_metadata.rs` but are not
in the representative test set. They all use the same OpenAI-compatible adapter
path proven by the representative set above. Configuration follows the same
pattern: set the env var, use `--provider <id>`.

| Provider | Base URL | Auth Env Key |
|---|---|---|
| 302ai | `https://api.302.ai/v1` | `302AI_API_KEY` |
| abacus | `https://routellm.abacus.ai/v1` | `ABACUS_API_KEY` |
| aihubmix | `https://aihubmix.com/v1` | `AIHUBMIX_API_KEY` |
| berget | `https://api.berget.ai/v1` | `BERGET_API_KEY` |
| chutes | `https://llm.chutes.ai/v1` | `CHUTES_API_KEY` |
| cortecs | `https://api.cortecs.ai/v1` | `CORTECS_API_KEY` |
| friendli | `https://api.friendli.ai/serverless/v1` | `FRIENDLI_TOKEN` |
| helicone | `https://ai-gateway.helicone.ai/v1` | `HELICONE_API_KEY` |
| inference | `https://inference.net/v1` | `INFERENCE_API_KEY` |
| nano-gpt | `https://nano-gpt.com/api/v1` | `NANO_GPT_API_KEY` |
| novita-ai | `https://api.novita.ai/openai` | `NOVITA_API_KEY` |
| poe | `https://api.poe.com/v1` | `POE_API_KEY` |
| requesty | `https://router.requesty.ai/v1` | `REQUESTY_API_KEY` |
| siliconflow | `https://api.siliconflow.com/v1` | `SILICONFLOW_API_KEY` |
| venice | `https://api.venice.ai/api/v1` | `VENICE_API_KEY` |
| vultr | `https://api.vultrinference.com/v1` | `VULTR_API_KEY` |
| wandb | `https://api.inference.wandb.ai/v1` | `WANDB_API_KEY` |

## Deferred Providers (53 total)

Providers listed here have metadata entries in `provider-parity-checklist.json`
but are explicitly deferred from the quick-win wave. Each entry includes the
deferral rationale and user impact assessment.

### Native Adapter Required (no OpenAI-compatible path)

These providers use proprietary protocols or auth flows that cannot route
through the OpenAI-compatible adapter. A dedicated adapter module is needed.

| Provider | Rationale | User Impact |
|---|---|---|
| v0 | No validated protocol/auth route | Users cannot access v0 models via Pi |
| gitlab | Proprietary auth flow (GitLab Duo) | GitLab Duo users must use GitLab's own tooling |
| llama | No confirmed API/auth contract | Meta's Llama API not yet supported |
| lmstudio | Localhost only, no cloud API | Works via `models.json` base_url override for local use |
| ollama (local) | No auth required, localhost only | Works via preset for local use; no remote testing |

### Regional CN Variants

Chinese-region variants require separate endpoint and auth verification.
The global parent provider works; the CN variant does not yet.

| Provider | Rationale | User Impact |
|---|---|---|
| alibaba-cn | Regional CN variant of alibaba | CN users should use alibaba provider with CN endpoint override |
| moonshotai-cn | Regional CN variant of moonshotai | CN users should use moonshotai with CN endpoint override |
| siliconflow-cn | Regional CN variant of siliconflow | CN users should use siliconflow with CN endpoint override |
| minimax-cn | Regional endpoint/auth not verified | CN users cannot use minimax CN variant yet |
| minimax-cn-coding-plan | CN coding-plan variant not verified | Not available |

### Coding-Plan Variants

Distinct coding-plan style IDs not currently represented in runtime routing.
The base provider works; the specialized variant does not.

| Provider | Rationale | User Impact |
|---|---|---|
| kimi-for-coding | Coding-plan ID not in routing | Users can use moonshotai provider with standard models |
| minimax-coding-plan | Coding-plan variant not verified | Users can use minimax base models |
| zai-coding-plan | Coding-plan variant not verified | Not available |
| zhipuai-coding-plan | Coding-plan variant not verified | Not available |

### No Runtime/Auth Evidence

These providers appear in upstream catalogs but lack verified protocol, auth,
or endpoint contracts. They may be onboarded as quick-wins once evidence is
gathered.

| Provider | Rationale | User Impact |
|---|---|---|
| 302ai | No in-repo runtime/auth evidence | Not available; set `302AI_API_KEY` when supported |
| abacus | No in-repo runtime/auth evidence | Not available |
| aihubmix | No in-repo runtime/auth evidence | Not available |
| bailing | No runtime/auth evidence | Not available |
| baseten | No runtime/auth evidence | Not available |
| berget | No runtime/auth evidence | Not available |
| chutes | No runtime/auth evidence | Not available |
| cortecs | No runtime/auth evidence | Not available |
| firmware | No runtime/auth evidence | Not available |
| friendli | No runtime/auth evidence | Not available |
| huggingface | No validated runtime/auth contract | Not available as named provider |
| iflowcn | No runtime/auth evidence | Not available |
| inception | No runtime/auth evidence | Not available |
| inference | No runtime/auth evidence | Not available |
| io-net | No runtime/auth evidence | Not available |
| jiekou | No runtime/auth evidence | Not available |
| lucidquery | No runtime/auth evidence | Not available |
| minimax | No validated provider protocol | Not available |
| moark | No runtime/auth evidence | Not available |
| modelscope | No validated protocol/auth route | Not available |
| morph | No runtime/auth evidence | Not available |
| nano-gpt | No runtime/auth evidence | Not available |
| nebius | No validated protocol/auth route | Not available |
| nova | No runtime/auth evidence | Not available |
| novita-ai | No validated protocol/auth route | Not available |
| nvidia | No validated protocol/auth route | Tested via representative set but not in parity checklist |
| ovhcloud | No validated protocol/auth route | Not available |
| poe | No validated protocol/auth route | Not available |
| privatemode-ai | No runtime/auth evidence | Not available |
| scaleway | No validated protocol/auth route | Not available |
| siliconflow | No validated protocol/auth route | Metadata-only; may work with env key |
| submodel | No validated protocol/auth route | Not available |
| synthetic | No validated protocol/auth route | Not available |
| upstage | No validated protocol/auth route | Not available |
| venice | No validated protocol/auth route | Metadata-only; may work with env key |
| vivgrid | No validated protocol/auth route | Not available |
| vultr | No validated protocol/auth route | Metadata-only; may work with env key |
| wandb | No validated protocol/auth route | Metadata-only; may work with env key |
| xiaomi | No validated protocol/auth route | Not available |
| zai | No validated protocol/auth route | Not available |
| zhipuai | No validated protocol/auth route | Not available |

### Workaround for Deferred Providers

Users needing a deferred provider can use `models.json` to configure it
manually if the provider supports OpenAI-compatible endpoints:

```json
{
  "providers": {
    "custom-provider": {
      "apiKey": "your-key",
      "models": {
        "custom-model": {
          "provider": "custom-provider",
          "api": "openai-completions",
          "base_url": "https://api.example.com/v1",
          "context_window": 128000,
          "max_tokens": 4096
        }
      }
    }
  }
}
```

## CI Integration

- Provider gap test matrix: `docs/provider-gaps-test-matrix.json`
- CI gate: Gate 12 in `tests/ci_full_suite_gate.rs` validates the test matrix
- Artifact retention: 30-day retention for shard artifacts in `.github/workflows/ci.yml`
- Suite classification: All test files listed in `tests/suite_classification.toml`
- Registry guardrails: `tests/provider_registry_guardrails.rs` prevents silent upstream drift
- Parity checklist: `docs/provider-parity-checklist.json` (99 entries, 53 deferred)

## Decision Audit Trail

| Decision | Evidence | Bead |
|---|---|---|
| 60+ providers routed via OpenAI-compatible adapter | `src/provider_metadata.rs` metadata entries | bd-3uqg.11.10.2 |
| 7 representative longtail providers tested | 56 contract + 49 conformance + 101 E2E tests | bd-3uqg.11.10.5, bd-3uqg.11.10.6 |
| 53 providers deferred with rationale | `docs/provider-parity-checklist.json` deferred entries | bd-3uqg.11.10.3 |
| Registry guardrails prevent silent drift | `tests/provider_registry_guardrails.rs` | bd-3uqg.11.10.4 |
