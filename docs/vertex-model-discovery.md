# Google Vertex Model Garden discovery (draft, not compiler-verified)

This branch adds a read-only Model Garden publisher-listing adapter for the `google-vertex` provider. The native list API is **not** an OpenAI-compatible `/v1/models` endpoint. Results are publisher catalog **candidates**, not proof that a selected project can invoke a model, or that the model supports Pi's inference transport.

## Scope and authentication

- Explicit `GOOGLE_CLOUD_PROJECT` (or `VERTEX_PROJECT`) is required and validated. `GOOGLE_CLOUD_LOCATION` / `VERTEX_LOCATION` chooses the regional endpoint (default `us-central1`).
- Lists `GET /v1beta1/publishers/google/models` and `GET /v1beta1/publishers/anthropic/models` at the canonical `{location}-aiplatform.googleapis.com` host (the `global` location uses `aiplatform.googleapis.com`). The project is supplied as the `x-goog-user-project` quota context header. These publisher listings are not project-filtered entitlements.
- Uses the existing explicit Google access-token or custom Authorization header path. This adapter **does not** implement ADC/service-account token exchange. It refuses arbitrary custom discovery URLs, unknown route headers, pagination cycles and oversized/unparseable results; error messages exclude response bodies and credentials.
- Returns only Gemini-family Google and Claude-family Anthropic IDs; this is a deliberately conservative name filter, **not** a verified model-capability mapping.
- Does not write account/project results to the generated catalog or reuse the general cross-account process cache. The periodic persisted-catalog sync intentionally skips Google Vertex. Use explicit model fetching and keep manual `models.json` overrides authoritative.

## Known limitations

The live model list is not guaranteed to include every generative model/version. Not all listed Model Garden entries are available to the configured project, and IDs may require adaptation to inference route naming. The adapter presently treats listing failures from either publisher as failure of the combined discovery, then falls back to static models for non-strict fetches. A live provider account is required to confirm these choices.

## Verification checklist — not yet run

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
./scripts/e2e/run_all.sh --profile ci
cargo test --all-targets --features ext-conformance
dsr quality --tool pi_agent_rust
```

Run the model-fetch unit tests and authenticated Vertex project/location integration tests in a controlled environment; verify disabled APIs, expired credentials, pagination and user-project quota attribution. Do not promote a draft PR solely based on static code review.
