# Bedrock model discovery (draft / verification required)

Pi's `amazon-bedrock` provider now has a native, read-only Bedrock control-plane discovery path. It does **not** call the inference endpoint as if it exposed OpenAI's `/v1/models`.

## Intended behavior

- Use the existing Amazon Bedrock inference provider's AWS credential resolver (ambient keys, profiles/SSO and stored credentials). An explicit `--api-key` may select the provider's existing bearer-token mode.
- Read `GET https://bedrock.{region}.amazonaws.com/foundation-models`, retaining only model IDs that advertise streaming text output and on-demand inference, excluding end-of-life models.
- Read `GET https://bedrock.{region}.amazonaws.com/inference-profiles`, retaining active inference profile IDs, following `nextToken` up to a bounded 16 pages.
- Reuse the existing SigV4 signer for a **GET** canonical request; signed AWS headers are owned by Pi. Never send AWS credentials to a custom `models.json` `baseUrl` for this native discovery path.
- On explicit refresh, return an error for an empty, malformed, unauthorized, or incomplete listing. On ordinary fetch, continue to allow static fallback.
- Do **not** persist or process-cache regional Bedrock discovery. Results depend on the active AWS account and region; the existing fetched-model catalog binding cannot safely rebind this membership across account changes. The periodic persisted-catalog synchronizer therefore skips Bedrock. Call an explicit model-list refresh for a fresh in-memory result.

## Limitations

This is discovery, **not** verification of entitlement to invoke each listed model/profile using ConverseStream. The list intentionally excludes provisioned-only or non-streaming foundation models. Model IDs from inference profiles may point to models with additional restrictions. Inference availability, account permissions, throttling and regional differences must be validated with real credentials.

Google Vertex AI project/location-scoped discovery is not implemented by this patch.

## Required acceptance checks

This work has not been compiled or executed in the present environment. Before leaving draft:

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
./scripts/e2e/run_all.sh --profile ci
cargo test --all-targets --features ext-conformance
dsr quality --tool pi_agent_rust
```

Then use live sandbox credentials to test the regional AWS control-plane listing, a profile with SSO credentials, expired/unauthorized credentials, and a known inference profile. Keep credentials, account IDs, and unredacted provider response bodies out of test logs and artifacts.
