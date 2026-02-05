# Popular Pi Extensions — Scoring + Inclusion Criteria (bd-29ko)

This document defines what **“popular”** means for Pi extensions and how we decide whether an
extension belongs in the **expanded compatibility corpus** (see `bd-po15` / `bd-d7gn`).

Goal: make selection **mechanical and reproducible** so two people applying the rubric to the same
inputs would converge on the same tiering, while explicitly accounting for **compatibility** and
**reliability risk**.

Non-goals:
- This is **not** the stratified conformance sample (that’s the pinned 16 in `docs/extension-sample.json`).
- This does not prescribe *how* to scrape data; it prescribes **what fields and scoring logic**
  the inventory must produce.

---

## 0) Definitions

- **Candidate**: an extension artifact (single file, directory, package) plus provenance metadata.
- **Corpus**: the tiered set we aim to support **unmodified**.
  - **Tier-1**: must-pass, always run in CI.
  - **Tier-2**: stretch; run in CI on a schedule or as an opt-in job.
- **Signals**: objective measures like stars/downloads/official listings.
- **Coverage**: how much *new* extension surface area this exercises (runtime tier, interaction
  model, hostcall mix).

---

## 1) Hard Gates (must-pass to be in Tier-1/Tier-2)

If a candidate fails any gate, it is **Excluded** (or kept as “metadata-only” for research).

### 1.1 Provenance is pin-able

The candidate must have a stable, reproducible reference:
- Git repo: commit SHA (preferred) or immutable tag + repo URL
- Gist: gist revision SHA / commit SHA + URL
- npm: package name + exact version

No floating refs (e.g. “main”, “latest”, unpinned URLs).

### 1.2 License / redistribution is known

We must be able to answer: “Can we vendor this in `tests/ext_conformance/artifacts/**`?”

Allowed outcomes:
- **OK**: MIT/Apache-2.0/BSD/etc or explicit permission
- **Restricted**: redistributable but with constraints (document)
- **Exclude**: unknown / proprietary / unclear

Tier-1/Tier-2 requires **OK** or **Restricted** with a concrete plan.

### 1.3 Unmodified compatibility (normative)

Tier-1/Tier-2 extensions must run **without manual source edits**.

Allowed:
- Deterministic compilation/transpilation (TS → JS)
- Deterministic bundling (multi-file → single artifact)
- Deterministic import/specifier rewrites (e.g. Node builtins → `pi:node/*`)
- Pi-provided shims/connectors in the runtime (not per-extension hacks)

Not allowed:
- Editing the extension’s logic “just for Pi”
- Per-extension special cases in the runtime (“if extension_id == X …”)
- Patching emitted bundles post-hoc in a non-auditable way

See `docs/ext-compat.md` (“Unmodified compatibility”) for the full contract.

### 1.4 Determinism + reproducibility

Tier-1/Tier-2 candidates must have at least one scenario that can run deterministically:
- no real OAuth login, no real API keys, no flaky network requirements
- if the extension is network-heavy, it must have an offline/error-path scenario or be VCR-able

---

## 2) Scoring Rubric (Base 0–100 + Risk Penalty)

We compute a **base score (0–100)** and then subtract a **reliability‑risk penalty (0–15)**.

**Base score = Popularity + Adoption + Coverage + Activity + Compatibility.**  
**Final score = Base score – Risk penalty (floor at 0).**

| Sub-score | Weight | What it measures |
|---|---:|---|
| Popularity | 30 | “How broadly visible is it?” |
| Adoption | 15 | “Do real users install/use it?” |
| Coverage | 20 | “How much new surface does it cover for our proof?” |
| Activity | 15 | “Is it maintained enough to matter today?” |
| Compatibility | 20 | “How close is it to unmodified compatibility?” |

The inventory should store **all inputs** used to compute the score and a short rationale.

### 2.1 Popularity (0–40)

Add up the following components (cap at 40):

1) **Official / first-party visibility (0–15)**
- Listed on `buildwithpi.ai/packages` (or official docs): +15
- Shipped as a pi-mono example extension: +10
- badlogic-authored gist referenced by official docs: +8

2) **GitHub stars (0–15)** (repo or gist mirror if applicable)
- ≥ 5,000: +15
- ≥ 2,000: +13
- ≥ 1,000: +11
- ≥ 500: +9
- ≥ 200: +7
- ≥ 50: +4
- otherwise: +0

3) **Community references (0–10)** (count distinct sources)
- ≥ 10 distinct references: +10
- ≥ 5: +7
- ≥ 2: +4
- otherwise: +0

Examples of “references”: other repos linking it, blog posts, curated lists, Discord snippets with
links. (The inventory must capture URLs.)

### 2.2 Adoption (0–20)

Pick the best available signals for the source type; store raw numbers.

1) **npm downloads (0–12)** (if published on npm)
- ≥ 50k / month: +12
- ≥ 10k / month: +10
- ≥ 2k / month: +7
- ≥ 500 / month: +4
- otherwise: +0

2) **Forks / derivative usage (0–8)** (GitHub)
- forks ≥ 500: +8
- ≥ 200: +6
- ≥ 50: +3
- otherwise: +0

If neither npm nor forks apply, score Adoption as 0 and note “signal unavailable”.

### 2.3 Coverage (0–25)

Coverage is about maximizing proof value, not “popularity”. Score by tags:

1) **Runtime tier (0–8)**
- pkg-with-deps: +8
- multi-file: +6
- legacy-js (single file): +4

2) **Interaction model breadth (0–9)**
- provider: +4
- ui_integration: +2
- event_hook: +2
- slash_command: +1
- tool_only: +1

(cap at 9; use tags as in `docs/EXTENSION_SAMPLING_MATRIX.md`.)

3) **Hostcall mix (0–8)** (capabilities used)
- uses `exec`: +2
- uses `http`: +2
- uses `read`/`write`/`edit`: +2
- uses `ui`: +1
- uses session mutation APIs: +1

### 2.4 Activity / Recency (0–15)

Use the most relevant date for the source type (repo last commit, npm publish, gist update).

- Updated ≤ 30 days: +15
- ≤ 90 days: +12
- ≤ 180 days: +9
- ≤ 365 days: +6
- ≤ 730 days: +3
- otherwise: +0

### 2.5 Compatibility (0–20)

Compatibility is a **positive score** (not just a gate) so selection can favor
extensions that are already close to unmodified parity.

Suggested scoring (pick best matching level, then adjust ±2 for nuance):

- **20** — Unmodified, passes static scan, no forbidden APIs, no extension‑specific shims.
- **15** — Unmodified, but requires **generic** shims/rewrites (Node core, `pi:*` shims).
- **10** — Unmodified but depends on **incomplete generic runtime features** (e.g. provider hooks
  not fully wired yet); still plausible to land via runtime work.
- **0** — Requires per‑extension edits or fails compatibility gates (blocked).

### 2.6 Reliability Risk Penalty (0–15)

Risk is a **penalty** (subtracted from base score). It captures “how likely this
extension will be flaky, non‑deterministic, or expensive to support in CI.”

Suggested penalty bands:

- **0** — Deterministic, minimal deps, no network or fully VCR‑able.
- **5** — Moderate deps or network use, but reproducible with mocks/VCR.
- **10** — High risk: OAuth flows, heavy UI timing sensitivity, large dep trees.
- **15** — Critical risk: native binaries, non‑deterministic side effects, unclear license.

---

## 3) Tiering Rules

Tiering uses both gates and score thresholds:

| Tier | Requirements |
|---|---|
| Tier-1 | Pass all gates + **final score** ≥ 70 |
| Tier-2 | Pass all gates + **final score** ≥ 50 |
| Excluded | Fails a gate OR final score < 50 |

Tie-breakers (when selecting a fixed-size set):
1) prefer higher **Coverage** score (proof value)
2) then higher **Popularity**
3) then more recent

---

## 4) Inventory Fields (schema guidance for bd-1o8j / bd-hhzv / bd-34io)

The candidate inventory should be representable as JSON objects with these fields:

```json
{
  "id": "stable-id",
  "name": "display name",
  "source": {
    "kind": "repo|gist|npm|pi-mono|buildwithpi",
    "url": "https://…",
    "repo": "owner/name",
    "commit": "sha-or-tag",
    "path": "path/inside/repo",
    "npm": { "name": "pkg", "version": "1.2.3" }
  },
  "license": { "spdx": "MIT", "redistribution": "ok|restricted|exclude", "notes": "" },
  "tags": {
    "runtime": "legacy-js|multi-file|pkg-with-deps|provider-ext",
    "interaction": ["tool_only","slash_command","event_hook","ui_integration","provider"],
    "capabilities": ["read","write","edit","exec","http","ui","session"]
  },
  "signals": {
    "github_stars": 0,
    "github_forks": 0,
    "npm_downloads_month": 0,
    "references": ["https://…"]
  },
  "recency": { "updated_at": "2026-01-31T00:00:00Z" },
  "compat": {
    "unmodified_required": true,
    "blocked_reasons": [],
    "required_shims": ["pi:node/fs", "pi:node/path"]
  },
  "score": {
    "popularity": 0,
    "adoption": 0,
    "coverage": 0,
    "activity": 0,
    "compatibility": 0,
    "risk_penalty": 0,
    "base_total": 0,
    "final_total": 0,
    "tier": "tier-1|tier-2|excluded",
    "rationale": "1-3 sentences explaining the score.",
    "risk_notes": "Optional: why the risk penalty was applied."
  }
}
```

Notes:
- `required_shims` is descriptive (what the extension appears to need), not a per-extension hack list.
- `blocked_reasons` must be objective and actionable (e.g. “license unknown”, “requires Node C++ addon”).
