# Published release consumer checks

This purpose is to verify selected publicly shipped bytes and their offline CLI
surface independently of the source checkout. DSR remains the sole quality,
build and release authority; this check does not qualify current source or a
whole release platform matrix.

Requirements: Python 3, authenticated read-only `gh api`, and `minisign` on PATH.
The verifier uses the repository's pinned epoch-2 public key, peels the requested
tag to an explicitly supplied source commit, checks the signed manifest and full
asset inventory, then verifies each requested payload, sidecar and signature.
GitHub size/digest metadata and before/after release identity must also match.

For example, verify published v0.7.1 on Apple Silicon:

```bash
python3 scripts/verify_published_release.py \
  --tag v0.7.1 --commit 403573bb42966d20032e7e1abd1d4417e1493814 \
  --asset pi_darwin_arm64 --output /path/to/new-evidence-directory --smoke
```

The output path must not exist, and its parent must already exist. Every download,
metadata response and command log is retained alongside `receipt.json`; reruns
need a new directory. No shell installer, self-updater, source build, credential
load, model request or artifact cleanup is performed. Asset redirects are checked
before following them, and TLS verification remains enabled.

`--smoke` uses the raw signed payload for the current host, an isolated environment
and fresh HOME/XDG directories. It checks exact version, static offline providers,
and the safe extension-policy JSON result. These fast read-only routes do not
prove token counting, interactive behavior, provider transport, installation or
upgrade. Omit `--smoke` to verify cross-platform payloads without executing them.

The reusable checks came from the retained October 2026 publication harness.
Machine-specific syscall interposers, worker paths and release-wave orchestration
are deliberately outside this portable consumer check.
