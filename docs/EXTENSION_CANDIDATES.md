## Raw Extension Candidate List (Unfiltered)

This is a **source-first, unfiltered** candidate inventory for extension sampling. It prioritizes **breadth of feature coverage** over popularity and will be refined by downstream sampling beads.

### Sources (where candidates come from)

1. **pi‑mono example extensions** (local repo snapshot; upstream commit snapshot for reference)  
   `legacy_pi_mono_code/pi-mono/packages/coding-agent/examples/extensions/README.md`  
   Upstream snapshot: https://upd.dev/badlogic/pi-mono/src/commit/c6fc084534d0091e6243bdcf929249e48c36c9e9/packages/coding-agent/examples/extensions/README.md  
   Repo: https://github.com/badlogic/pi-mono  

2. **pi‑mono local `.pi/extensions`** (seed extensions in repo)  
   `legacy_pi_mono_code/pi-mono/.pi/extensions/`  

3. **Official Pi site** (docs + packages)  
   https://buildwithpi.ai/  
   https://buildwithpi.ai/packages  

4. **badlogic GitHub gists (extensions)**  
   https://gist.github.com/badlogic  
   https://gist.github.com/badlogic/679b221a1749353a5be3f3134c120685  
   https://gist.github.com/badlogic/30aef35d686483ffce22cc2aad99f3ff  
   https://gist.github.com/badlogic/587bcbc5d1d2b4d1cf30a1d0756275b9  
   https://gist.github.com/badlogic/8273f2bff572272e1036887e0744c3c8  

5. **Community GitHub gists**  
   https://gist.github.com/nicobailon/ee8a65353b9103ad5d149e7eeb452b10  
   https://gist.github.com/aadishv/7615082df075519d6efd9de793aa860a  

6. **Community npm package w/ Pi extension integration**  
   https://www.npmjs.com/package/agentsbox  

7. **Claude Code plugin directories / marketplaces (curated lists)**  
   https://www.claudedirectory.org/  
   https://www.claudeindex.com/

> Note: npm “pi-package” keyword results and buildwithpi package listings are not enumerated here; source list provides where to search.

---

## GitHub / Community Sweep (bd‑3jxt) — Initial Pass (2026‑02‑05)

This is a **high‑signal, non‑exhaustive** snapshot of extension ecosystems discovered via
GitHub topic pages (`claude-code-plugin`, `claude-code-plugins`) plus official Pi sources and
curated community lists. "Updated" reflects the GitHub repo `updated_at` field (UTC).
Release tags are the latest GitHub release when present. **Categories/notes are inferred**
from repo names/descriptions and should be validated in follow‑up.

| Repo | Category | Stars / Forks | Updated (UTC) | License | Latest Release | Notes |
|---|---|---:|---|---|---|---|
| `badlogic/pi-mono` | Official repo | 6,977 / 717 | 2026‑02‑05 | MIT | v0.51.6 | — |
| `wshobson/agents` | Community repo | 27,847 / 3,068 | 2026‑02‑05 | MIT | none | — |
| `timescale/pg-aiguide` | Community repo | 1,501 / 77 | 2026‑02‑05 | Apache‑2.0 | v0.3.0 | — |
| `jeremylongshore/claude-code-plugins-plus-skills` | Community repo | 1,285 / 155 | 2026‑02‑05 | NOASSERTION | v4.14.0 | — |
| `kenryu42/claude-code-safety-net` | Community repo | 972 / 42 | 2026‑02‑05 | MIT | v0.7.1 | — |
| `gmickel/gmickel-claude-marketplace` | Community repo | 501 / 33 | 2026‑02‑05 | MIT | flow-next‑v0.20.19 | — |
| `ccplugins/awesome-claude-code-plugins` | Curated list | 440 / 65 | 2026‑02‑05 | Apache‑2.0 | none | — |
| `fcakyon/claude-codex-settings` | Community repo | 401 / 39 | 2026‑02‑05 | Apache‑2.0 | v2.1.0 | — |
| `quemsah/awesome-claude-plugins` | Curated list | 89 / 4 | 2026‑02‑05 | NONE | none | — |
| `vincenthopf/My-Claude-Code` | Curated list | 127 / 3 | 2026‑02‑02 | NOASSERTION | none | — |
| `steipete/claude-code-mcp` | Community repo | 1,073 / 128 | 2026‑02‑05 | MIT | v1.10.2 | MCP server |
| `siteboon/claudecodeui` | Community repo | 6,018 / 787 | 2026‑02‑05 | GPL‑3.0 | v1.16.3 | UI wrapper |
| `disler/claude-code-hooks-mastery` | Community repo | 2,534 / 509 | 2026‑02‑05 | NONE | none | Hooks |
| `hesreallyhim/awesome-claude-code` | Curated list | 22,903 / 1,319 | 2026‑02‑05 | NOASSERTION | none | — |
| `ComposioHQ/awesome-claude-skills` | Curated list | 30,633 / 2,921 | 2026‑02‑05 | NONE | none | — |

### Topic sweep: `pi-agent`, `pi-coding-agent`, `pi-extension` (long‑tail)

| Repo | Category | Stars / Forks | Updated (UTC) | License | Latest Release | Notes |
|---|---|---:|---|---|---|---|
| `Piebald-AI/splitrail` | Community repo | 100 / 10 | 2026‑02‑05 | MIT | v3.3.1 | — |
| `qualisero/awesome-pi-agent` | Curated list | 49 / 5 | 2026‑02‑05 | MIT | none | — |
| `tmustier/pi-extensions` | Community repo | 35 / 4 | 2026‑02‑05 | MIT | pi-skill-creator/v0.2.0 | — |
| `nicobailon/pi-web-access` | Community repo | 34 / 1 | 2026‑02‑05 | MIT | v0.7.2 | — |
| `tmustier/pi-nes` | Community repo | 13 / 1 | 2026‑02‑03 | MIT | v0.2.36 | — |
| `ben-vargas/pi-packages` | Community repo | 7 / 1 | 2026‑02‑05 | MIT | none | — |
| `Graffioh/pi-super-curl` | Community repo | 3 / 0 | 2026‑02‑05 | MIT | none | — |
| `imsus/pi-extension-minimax-coding-plan-mcp` | Community repo | 0 / 0 | 2026‑01‑29 | MIT | v1.0.0 | — |

### Repo search logs (bd‑kgmr) — expanded

```json
{
  "executed_at": "2026-02-05T17:29:10Z",
  "queries": [
    {
      "query": "topic:pi-agent",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "Piebald-AI/splitrail", "stars": 100, "forks": 10, "updated_at": "2026-02-05T12:17:59Z", "license": "mit", "url": "https://github.com/Piebald-AI/splitrail"},
        {"repo": "qualisero/awesome-pi-agent", "stars": 49, "forks": 5, "updated_at": "2026-02-05T10:28:25Z", "license": "mit", "url": "https://github.com/qualisero/awesome-pi-agent"},
        {"repo": "qualisero/rhubarb-pi", "stars": 2, "forks": 0, "updated_at": "2026-01-25T22:30:56Z", "license": "mit", "url": "https://github.com/qualisero/rhubarb-pi"},
        {"repo": "Dwsy/ace-tool-skill", "stars": 0, "forks": 0, "updated_at": "2026-01-23T01:23:44Z", "license": "mit", "url": "https://github.com/Dwsy/ace-tool-skill"},
        {"repo": "Dwsy/knowledge-builder-extension", "stars": 0, "forks": 0, "updated_at": "2026-01-07T14:04:16Z", "license": "", "url": "https://github.com/Dwsy/knowledge-builder-extension"}
      ]
    },
    {
      "query": "topic:pi-extension",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "ben-vargas/pi-packages", "stars": 7, "forks": 1, "updated_at": "2026-02-05T07:04:12Z", "license": "mit", "url": "https://github.com/ben-vargas/pi-packages"},
        {"repo": "Graffioh/pi-super-curl", "stars": 3, "forks": 0, "updated_at": "2026-02-05T09:31:40Z", "license": "mit", "url": "https://github.com/Graffioh/pi-super-curl"},
        {"repo": "default-anton/pi-moonshot", "stars": 1, "forks": 0, "updated_at": "2026-01-27T19:28:10Z", "license": "", "url": "https://github.com/default-anton/pi-moonshot"},
        {"repo": "default-anton/pi-subdir-context", "stars": 1, "forks": 0, "updated_at": "2026-01-29T20:13:06Z", "license": "mit", "url": "https://github.com/default-anton/pi-subdir-context"},
        {"repo": "imsus/pi-extension-minimax-coding-plan-mcp", "stars": 0, "forks": 0, "updated_at": "2026-01-29T14:54:38Z", "license": "mit", "url": "https://github.com/imsus/pi-extension-minimax-coding-plan-mcp"},
        {"repo": "juanibiapina/pi-gob", "stars": 0, "forks": 0, "updated_at": "2026-02-04T14:54:47Z", "license": "mit", "url": "https://github.com/juanibiapina/pi-gob"},
        {"repo": "gturkoglu/pi-dynsys", "stars": 0, "forks": 0, "updated_at": "2026-02-04T23:13:42Z", "license": "mit", "url": "https://github.com/gturkoglu/pi-dynsys"},
        {"repo": "juanibiapina/pi-files", "stars": 0, "forks": 0, "updated_at": "2026-02-04T07:50:39Z", "license": "mit", "url": "https://github.com/juanibiapina/pi-files"}
      ]
    },
    {
      "query": "topic:pi-coding-agent",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "tmustier/pi-extensions", "stars": 35, "forks": 4, "updated_at": "2026-02-05T13:32:49Z", "license": "mit", "url": "https://github.com/tmustier/pi-extensions"},
        {"repo": "nicobailon/pi-web-access", "stars": 34, "forks": 1, "updated_at": "2026-02-05T16:46:52Z", "license": "mit", "url": "https://github.com/nicobailon/pi-web-access"},
        {"repo": "tmustier/pi-nes", "stars": 13, "forks": 1, "updated_at": "2026-02-03T22:50:24Z", "license": "mit", "url": "https://github.com/tmustier/pi-nes"},
        {"repo": "mxyhi/ok-skills", "stars": 3, "forks": 0, "updated_at": "2026-02-04T04:09:08Z", "license": "apache-2.0", "url": "https://github.com/mxyhi/ok-skills"},
        {"repo": "gturkoglu/pi-codex-apply-patch", "stars": 2, "forks": 0, "updated_at": "2026-02-02T05:27:32Z", "license": "mit", "url": "https://github.com/gturkoglu/pi-codex-apply-patch"},
        {"repo": "otahontas/pi-coding-agent-catppuccin", "stars": 1, "forks": 0, "updated_at": "2026-02-03T22:57:39Z", "license": "", "url": "https://github.com/otahontas/pi-coding-agent-catppuccin"},
        {"repo": "zenobi-us/pi-rose-pine", "stars": 1, "forks": 1, "updated_at": "2026-02-03T04:24:53Z", "license": "mit", "url": "https://github.com/zenobi-us/pi-rose-pine"},
        {"repo": "imsus/pi-extension-minimax-coding-plan-mcp", "stars": 0, "forks": 0, "updated_at": "2026-01-29T14:54:38Z", "license": "mit", "url": "https://github.com/imsus/pi-extension-minimax-coding-plan-mcp"},
        {"repo": "gturkoglu/pi-dynsys", "stars": 0, "forks": 0, "updated_at": "2026-02-04T23:13:42Z", "license": "mit", "url": "https://github.com/gturkoglu/pi-dynsys"}
      ]
    },
    {
      "query": "buildwithpi extension",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": []
    },
    {
      "query": "\"pi-mono\" extension",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": []
    },
    {
      "query": "\"pi agent\" extension language:TypeScript",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "yulqen/conductor-pi", "stars": 2, "forks": 0, "updated_at": "2026-02-04T04:19:55Z", "license": "other", "url": "https://github.com/yulqen/conductor-pi"},
        {"repo": "rytswd/pi-agent-extensions", "stars": 2, "forks": 0, "updated_at": "2026-02-05T13:32:15Z", "license": "mit", "url": "https://github.com/rytswd/pi-agent-extensions"},
        {"repo": "lebonbruce/pi-hippocampus", "stars": 3, "forks": 1, "updated_at": "2026-02-03T11:52:06Z", "license": "mit", "url": "https://github.com/lebonbruce/pi-hippocampus"},
        {"repo": "byteowlz/pi-agent-extensions", "stars": 0, "forks": 0, "updated_at": "2026-01-30T08:26:00Z", "license": "", "url": "https://github.com/byteowlz/pi-agent-extensions"},
        {"repo": "charles-cooper/pi-extensions", "stars": 0, "forks": 0, "updated_at": "2026-01-28T14:54:33Z", "license": "mit", "url": "https://github.com/charles-cooper/pi-extensions"},
        {"repo": "Willyfrog/pi-agent-extensions", "stars": 0, "forks": 0, "updated_at": "2026-01-15T23:53:28Z", "license": "mit", "url": "https://github.com/Willyfrog/pi-agent-extensions"},
        {"repo": "Itsnotaka/dot-pi", "stars": 0, "forks": 0, "updated_at": "2026-02-05T07:22:43Z", "license": "", "url": "https://github.com/Itsnotaka/dot-pi"},
        {"repo": "LEUNGUU/pi-agent-config", "stars": 0, "forks": 0, "updated_at": "2026-01-20T07:17:03Z", "license": "", "url": "https://github.com/LEUNGUU/pi-agent-config"}
      ]
    },
    {
      "query": "\"pi agent\" extension language:JavaScript",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "Volantk/pi-agent-skills-extensions", "stars": 0, "forks": 0, "updated_at": "2026-02-04T09:12:25Z", "license": "", "url": "https://github.com/Volantk/pi-agent-skills-extensions"}
      ]
    },
    {
      "query": "\"Pi Agent\" extension",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": []
    },
    {
      "query": "\"pi coding agent\" extension",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "nicobailon/pi-interactive-shell", "stars": 109, "forks": 5, "updated_at": "2026-02-04T21:56:07Z", "license": "", "url": "https://github.com/nicobailon/pi-interactive-shell"},
        {"repo": "nicobailon/pi-model-switch", "stars": 9, "forks": 0, "updated_at": "2026-02-02T00:42:43Z", "license": "", "url": "https://github.com/nicobailon/pi-model-switch"},
        {"repo": "toorusr/ai-extensions", "stars": 0, "forks": 0, "updated_at": "2026-01-26T20:46:47Z", "license": "", "url": "https://github.com/toorusr/ai-extensions"},
        {"repo": "ferologics/pi-extensions", "stars": 1, "forks": 0, "updated_at": "2026-01-25T14:41:05Z", "license": "", "url": "https://github.com/ferologics/pi-extensions"},
        {"repo": "assagman/pi-extensions", "stars": 1, "forks": 0, "updated_at": "2026-01-30T20:47:06Z", "license": "mit", "url": "https://github.com/assagman/pi-extensions"},
        {"repo": "zenobi-us/pi-zk", "stars": 0, "forks": 0, "updated_at": "2026-01-31T14:32:02Z", "license": "mit", "url": "https://github.com/zenobi-us/pi-zk"},
        {"repo": "Istar-Eldritch/ai-tools", "stars": 0, "forks": 1, "updated_at": "2026-02-05T14:42:44Z", "license": "", "url": "https://github.com/Istar-Eldritch/ai-tools"},
        {"repo": "carsonfarmer/pi-extensions", "stars": 0, "forks": 0, "updated_at": "2026-02-05T05:27:40Z", "license": "", "url": "https://github.com/carsonfarmer/pi-extensions"},
        {"repo": "gturkoglu/pi-codex-apply-patch", "stars": 2, "forks": 0, "updated_at": "2026-02-02T05:27:32Z", "license": "mit", "url": "https://github.com/gturkoglu/pi-codex-apply-patch"},
        {"repo": "Istar-Eldritch/pi-wakatime", "stars": 0, "forks": 0, "updated_at": "2026-01-23T19:22:59Z", "license": "mit", "url": "https://github.com/Istar-Eldritch/pi-wakatime"}
      ]
    },
    {
      "query": "\"pi-coding-agent\" extension",
      "executed_at": "2026-02-05T17:29:10Z",
      "limit": 30,
      "results": [
        {"repo": "nicobailon/pi-interactive-shell", "stars": 109, "forks": 5, "updated_at": "2026-02-04T21:56:07Z", "license": "", "url": "https://github.com/nicobailon/pi-interactive-shell"},
        {"repo": "nicobailon/pi-model-switch", "stars": 9, "forks": 0, "updated_at": "2026-02-02T00:42:43Z", "license": "", "url": "https://github.com/nicobailon/pi-model-switch"},
        {"repo": "ferologics/pi-extensions", "stars": 1, "forks": 0, "updated_at": "2026-01-25T14:41:05Z", "license": "", "url": "https://github.com/ferologics/pi-extensions"},
        {"repo": "assagman/pi-extensions", "stars": 1, "forks": 0, "updated_at": "2026-01-30T20:47:06Z", "license": "mit", "url": "https://github.com/assagman/pi-extensions"},
        {"repo": "zenobi-us/pi-zk", "stars": 0, "forks": 0, "updated_at": "2026-01-31T14:32:02Z", "license": "mit", "url": "https://github.com/zenobi-us/pi-zk"},
        {"repo": "Istar-Eldritch/ai-tools", "stars": 0, "forks": 1, "updated_at": "2026-02-05T14:42:44Z", "license": "", "url": "https://github.com/Istar-Eldritch/ai-tools"},
        {"repo": "carsonfarmer/pi-extensions", "stars": 0, "forks": 0, "updated_at": "2026-02-05T05:27:40Z", "license": "", "url": "https://github.com/carsonfarmer/pi-extensions"},
        {"repo": "Istar-Eldritch/pi-wakatime", "stars": 0, "forks": 0, "updated_at": "2026-01-23T19:22:59Z", "license": "mit", "url": "https://github.com/Istar-Eldritch/pi-wakatime"},
        {"repo": "gturkoglu/pi-codex-apply-patch", "stars": 2, "forks": 0, "updated_at": "2026-02-02T05:27:32Z", "license": "mit", "url": "https://github.com/gturkoglu/pi-codex-apply-patch"}
      ]
    }
  ]
}
```

Follow‑ups:
- Resolve `NOASSERTION`/`NONE` license entries via LICENSE files or SPDX metadata.
- Expand coverage to other high‑signal topic pages (e.g., `claude-code-mcp`, `claude-code-hooks`).
- Several broad‑net queries returned zero results; plan to expand with code search + curated lists to reach target coverage.

---

## Discovery Playbook (Repeatable Queries) (bd‑19rf)

Goal: provide a deterministic checklist of **discovery channels + copy/paste queries** so future agents can repeat online research and converge on the same candidate set.

### A) Official Pi sources (baseline)

- `pi-mono` examples/extensions list (local snapshot):  
  `legacy_pi_mono_code/pi-mono/packages/coding-agent/examples/extensions/README.md`
- `pi-mono` seed extensions (local snapshot):  
  `legacy_pi_mono_code/pi-mono/.pi/extensions/`
- buildwithpi packages + docs:  
  https://buildwithpi.ai/  
  https://buildwithpi.ai/packages
- `badlogic` gists (extensions):  
  https://gist.github.com/badlogic

### B) GitHub repo discovery (keyword-based “broad net”)

Run as GitHub UI searches or via `gh search repos`. Record **date/time**, the exact query, and the number of candidate repos reviewed.

Suggested queries (tune language filters to reduce noise):

- `"buildwithpi" extension`
- `"pi-mono" extension`
- `"pi agent" extension language:TypeScript`
- `"pi agent" extension language:JavaScript`
- `"Pi Agent" extension`

`gh` examples:

```bash
gh search repos '"buildwithpi" extension' --limit 200
gh search repos '"pi-mono" extension' --limit 200
gh search repos '"pi agent" extension language:TypeScript' --limit 200
gh search repos '"pi agent" extension language:JavaScript' --limit 200
```

### C) GitHub code discovery (signature-based “find real entrypoints”)

Goal: find repositories that contain actual Pi extension registration code, not just mentions.

Suggested code search patterns (run each in GitHub Code Search or via `gh search code`):

- `registerTool(` (tools)
- `registerCommand(` (slash commands)
- `registerProvider(` (custom providers)
- `resources_discover` / `resourcesDiscover` (dynamic resources hooks)
- `tool_call` / `tool_result` / `turn_start` / `turn_end` (lifecycle events)

`gh` examples:

```bash
gh search code 'registerTool(' --limit 200
gh search code 'registerCommand(' --limit 200
gh search code 'registerProvider(' --limit 200
gh search code 'resources_discover' --limit 200
```

Validation heuristic (recommended): for each hit, confirm the repo has an extension entrypoint (e.g., a file exporting a default function that receives a Pi context object, or an obvious extension package layout).

### C1) GitHub code search log (bd‑3l39) — initial pass (2026‑02‑05)

Executed via `gh search code` (limit=100 unless noted). Result counts:

| Query | Result count |
|---|---:|
| `@mariozechner/pi-coding-agent` | 100 |
| `@mariozechner/pi-ai` | 100 |
| `registerTool(` | 100 |
| `registerCommand(` | 100 |
| `registerProvider(` | 100 |
| `ExtensionAPI` | 100 |
| `.pi/agent/extensions` | 9 |
| `"pi-extensions" "ExtensionAPI"` | 0 |

Validation pass (21 unique entrypoints; export‑default + registration/event hook observed):

| Repo | Entrypoint | Evidence |
|---|---|---|
| `openclaw/openclaw` | `.pi/extensions/redraws.ts` | `export default` + `registerCommand("tui")` |
| `mitsuhiko/agent-stuff` | `pi-extensions/loop.ts` | `export default` + `registerTool(` |
| `joelazar/dotfiles` | `dot_pi/agent/extensions/qna.ts` | `export default` + `registerCommand("qna")` |
| `w-winter/dot314` | `extensions/mac-system-theme.ts` | `export default` + `pi.on("session_start")` |
| `davidgasquez/dotfiles` | `agents/pi/extensions/branch-term.ts` | `export default` + `registerFlag(` |
| `pasky/pi-amplike` | `extensions/handoff.ts` | `export default` + `registerCommand("handoff")` |
| `mikeyobrien/rho` | `extensions/vault.ts` | `export default` + `pi.on("session_start")` |
| `mikeyobrien/rho` | `extensions/brain.ts` | `export default` + `pi.on("session_start")` |
| `hjanuschka/shitty-extensions` | `extensions/flicker-corp.ts` | `export default` + `registerCommand("flicker-corp")` |
| `hjanuschka/shitty-extensions` | `extensions/status-widget.ts` | `export default` + `pi.on("session_start")` |
| `hjanuschka/shitty-extensions` | `extensions/memory-mode.ts` | `export default` + `registerCommand("mem")` |
| `hjanuschka/shitty-extensions` | `extensions/plan-mode.ts` | `export default` + `registerFlag("plan")` |
| `hjanuschka/shitty-extensions` | `extensions/speedreading.ts` | `export default` + `registerCommand("speedread")` |
| `Mic92/dotfiles` | `home/.pi/agent/extensions/direnv.ts` | `export default` + `pi.on("session_start")` |
| `Mic92/dotfiles` | `home/.pi/agent/extensions/custom-footer.ts` | `export default` + `pi.on("session_start")` |
| `leiserfg/nix-config` | `home/leiserfg/pi-extensions/fzf.ts` | `export default` + `registerShortcut("ctrl+f")` |
| `leiserfg/nix-config` | `home/leiserfg/pi-extensions/notify.ts` | `export default` + `pi.on("agent_end")` |
| `zenobi-us/dotfiles` | `devtools/files/pi/agent/extensions/lsp/lsp.ts` | `export default` + `pi.on("session_start")` |
| `nexxeln/dots` | `config/pi/agent/extensions/review.ts` | `export default` + `registerCommand("review")` |
| `richardgill/nix` | `out-of-store-config/ai-agents/pi/extensions/process-info.ts` | `export default` + `pi.on("session_start")` |
| `default-anton/dotfiles` | `pi/agent/extensions/inject-context.impl.mjs` | `export default` + `pi.on("before_agent_start")` |

Notes / next pass:
- 6 queries hit the 100‑result cap; additional candidates remain unreviewed.
- Current validated count: **21 / 50** target. Next pass should validate remaining candidates from the queued list and add code‑search queries for `registerFlag(`, `registerShortcut(`, `registerMessageRenderer(`, plus `pi.registerTool(` with TS/JS language filters.

### D) npm discovery (distribution layer)

Goal: find npm packages that ship Pi extensions or integrate with Pi Agent.

Suggested queries (via npm UI or CLI):

```bash
npm search "pi agent extension" --json | jq '.[0:50] | map({name,version,description})'
npm search buildwithpi --json | jq '.[0:50] | map({name,version,description})'
npm search pi-mono --json | jq '.[0:50] | map({name,version,description})'
```

For each promising package, record popularity evidence (downloads, dependents) and extract any linked repo/gist.

### E) Marketplace ecosystems (OpenClaw / ClawHub)

Goal: locate any marketplace/directory indexes that list extensions in bulk.

Checklist:

- Identify the canonical OpenClaw org/repo and any associated “hub / marketplace / directory”.
- Prefer machine-readable indexes (JSON feeds, GraphQL endpoints, API responses) over scraping HTML.
- Export raw dumps with timestamps so inventory can be regenerated.

### F) Curated lists + cross-reference mining (mentions)

Goal: find “hidden” extensions referenced by other extension authors.

Suggested queries:

- GitHub repo search: `awesome "pi agent"` / `awesome buildwithpi` / `awesome pi-mono`
- GitHub code search across discovered repos: `pi extension`, `buildwithpi`, `pi-mono`, `registerTool(`, `registerCommand(`
- Issues/PR search in `pi-mono` and buildwithpi repos for “extension”, “packages”, “marketplace”

### Noise notes (practical filters)

- The query `pi extension` is usually too broad; add an anchor (`buildwithpi`, `pi-mono`, `registerTool(`).
- Prefer signature searches (`registerTool(` / `registerCommand(` / `registerProvider(`) to reduce false positives.
- When GitHub search results are noisy, filter by language (TS/JS first), and by last-updated recency.

---

## Candidate Metadata Fields

- **Name/Path**: extension name or directory.
- **Source**: where it originates (examples, gist, npm, git).
- **Type**: file, package directory, gist, npm package.
- **Interaction Model**: tool, slash command, event hook, provider, UI‑only, or mixed.
- **Capabilities (likely)**: `read` / `write` / `exec` / `http` / `env` (approximate from descriptions).
- **I/O Pattern**: FS‑heavy, network‑heavy, CPU‑heavy, or UI‑centric.
- **Last update**: from source listing where available; otherwise TBD.
- **Popularity score**: 0‑100 score (see rubric below).
- **Popularity evidence**: links/metrics backing the score (stars, downloads, docs mentions).
- **Compatibility status**: `unmodified` / `modified` / `blocked` (see requirements below).
- **Compatibility notes**: short reason when not `unmodified`.
- **Notes**: short rationale for inclusion.

> Capabilities are **inferred from descriptions**. A static scan can refine this later.

---

## Selection Scoring + Coverage Targets (bd‑3o8d)

This rubric defines **how we score and stratify** candidates for the Tier‑1/Tier‑2 corpus.
It extends popularity with **activity, compatibility, and reliability risk**. Full details
live in `docs/EXTENSION_POPULARITY_CRITERIA.md`; this is the selection‑focused summary.

### Selection Score (Base 0–100 + Risk Penalty)

**Base score = Popularity (30) + Adoption (15) + Coverage (20) + Activity (15) + Compatibility (20).**  
**Final score = Base score – Risk penalty (0–15).**

| Dimension | Points | How to Score |
|---|---:|---|
| **Popularity** | 0‑30 | Visibility: stars/forks, buildwithpi listings, npm downloads, curated mentions. |
| **Adoption** | 0‑15 | Evidence of real usage: docs/examples, references in multiple repos. |
| **Coverage** | 0‑20 | Unique surface area: interaction tags + capability diversity. |
| **Activity** | 0‑15 | Recency: ≤30d=15, ≤90d=12, ≤180d=9, ≤365d=6, ≤730d=3. |
| **Compatibility** | 0‑20 | Unmodified readiness: 20 (clean), 15 (needs generic shims), 10 (depends on incomplete generic runtime), 0 (blocked). |
| **Risk penalty** | 0‑15 | Subtract for high‑risk: OAuth‑heavy, native deps, non‑determinism, unclear license. |

Tiering (per `docs/EXTENSION_POPULARITY_CRITERIA.md`):
- **Tier‑1**: pass gates + **final score ≥ 70**
- **Tier‑2**: pass gates + **final score ≥ 50**
- **Excluded**: fails a gate or final score < 50  
Official pi‑mono examples are **always included**.

### Evidence Sources (non‑exhaustive)

- buildwithpi packages listing + install counts (if exposed)
- GitHub stars/forks + repo activity
- Gist stars/forks + last updated
- npm download stats (weekly/monthly)
- Mentions in official docs, examples, or community posts

### Unmodified Compatibility Requirements

**Unmodified** means the extension runs through the generic `extc` pipeline with **no per‑extension
source edits** and **no special‑case runtime shims**. Acceptable transforms are:

- Deterministic bundling/minification/TS→JS compilation
- Generic import rewrites (e.g., `node:*` → `pi:node/*`)
- Generic polyfills/shims provided by Pi (e.g., `pi:node/fs`, `process.env`, `Buffer`)
- Configuration via manifest or environment variables
- Deterministic test stubbing (VCR/network stubs) **without** modifying the extension source

**Not allowed** (moves candidate to `modified` or `blocked`):

- Editing extension source to remove/replace APIs
- Per‑extension compatibility patches or bespoke shims
- Node/Bun runtime dependencies or native addons
- Dynamic `require`/`eval` patterns that cannot be handled by generic rewrites

**Status definitions**

- `unmodified`: loads, registers, and can execute at least one scenario via generic pipeline
- `modified`: requires per‑extension edits or bespoke shims
- `blocked`: depends on unsupported/unsafe APIs that cannot be safely shimmed

### Coverage Targets (Tier‑1 must‑pass corpus)

Coverage targets are authoritative in `EXTENSIONS.md` (§1C.5). Summary:

- **Tier‑0 baseline**: official pi‑mono examples (must‑pass).
- **Tier‑1 MUST PASS**: **≥ 200** unmodified extensions, stratified across source tiers and behavior buckets.
- **Tier‑2 stretch**: long‑tail additions chosen for unique API surface (not popularity).

**Tier‑1 per‑source‑tier minimums (initial framing):**
`official-pi-mono` 60, `npm-registry` 50, `community` 50, `third-party-github` 20,
`agents-mikeastock` all available.

**Behavior / capability quotas (minimums):**
Include all provider‑registered + exec‑heavy extensions; ≥80 event hooks; ≥60 tool
registrations; ≥25 slash commands; ≥15 overlay‑heavy UI; ≥40 UI‑integrated; ≥25 network‑heavy;
≥50 FS‑heavy; ≥50 session/UI‑heavy combined.

### Machine‑Consumable Selection Output (required)

Selection output must be **machine‑consumable** so acquisition + conformance can run
without manual glue. Each selected candidate should carry:

- Stable ID + pinned source (repo SHA / npm version / gist rev)
- Tier (`tier‑0|tier‑1|tier‑2`) + score breakdown (base + risk penalty)
- Compatibility status (`unmodified|required_shims|blocked`) + rationale
- Coverage tags (runtime tier, interaction tags, capabilities)

---

## A) pi‑mono Example Extensions (local snapshot)

**Lifecycle & Safety**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `permission-gate.ts` | pi‑mono examples | file | event hook + UI | exec? | UI‑centric | Confirm dangerous bash commands. |
| `protected-paths.ts` | pi‑mono examples | file | event hook | write | FS‑heavy | Blocks writes to protected paths. |
| `confirm-destructive.ts` | pi‑mono examples | file | command + UI | env? | UI‑centric | Confirms destructive session actions. |
| `dirty-repo-guard.ts` | pi‑mono examples | file | event hook | exec | FS‑heavy | Prevents changes when git dirty. |
| `sandbox/` | pi‑mono examples | dir | tool hook + runtime | exec | FS/OS | OS‑level sandboxing. |

**Custom Tools**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `todo.ts` | pi‑mono examples | file | tool + command + UI | write | FS‑heavy | Todo tool + `/todos` with persistence. |
| `hello.ts` | pi‑mono examples | file | tool | none | UI‑centric | Minimal custom tool example. |
| `question.ts` | pi‑mono examples | file | tool + UI | env? | UI‑centric | `ctx.ui.select()` example. |
| `questionnaire.ts` | pi‑mono examples | file | tool + UI | env? | UI‑centric | Multi‑question UI flow. |
| `tool-override.ts` | pi‑mono examples | file | tool override | read/write | FS‑heavy | Wrap built‑ins for logging/ACL. |
| `truncated-tool.ts` | pi‑mono examples | file | tool | exec | FS‑heavy | Wrap ripgrep with truncation. |
| `antigravity-image-gen.ts` | pi‑mono examples | file | tool | http/write | network‑heavy | Image generation via HTTP. |
| `ssh.ts` | pi‑mono examples | file | tool | exec/http | network‑heavy | Delegate tools over SSH. |
| `subagent/` | pi‑mono examples | dir | tool + process | exec | CPU/FS | Delegates tasks to subagents. |

**Commands & UI**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `preset.ts` | pi‑mono examples | file | command | env | UI‑centric | Model/tool preset switching. |
| `plan-mode/` | pi‑mono examples | dir | command + UI | read | UI‑centric | Plan mode workflow. |
| `tools.ts` | pi‑mono examples | file | command + UI | env | UI‑centric | `/tools` enable/disable. |
| `handoff.ts` | pi‑mono examples | file | command | write | FS‑heavy | Handoff to new session. |
| `qna.ts` | pi‑mono examples | file | command + UI | env | UI‑centric | Extracts questions into editor. |
| `status-line.ts` | pi‑mono examples | file | UI | env | UI‑centric | Status updates. |
| `widget-placement.ts` | pi‑mono examples | file | UI | env | UI‑centric | Widget placement demo. |
| `model-status.ts` | pi‑mono examples | file | event hook + UI | env | UI‑centric | Model change status bar. |
| `snake.ts` | pi‑mono examples | file | UI | env | CPU/UI | Game w/ keyboard input. |
| `space-invaders.ts` | pi‑mono examples | file | UI | env | CPU/UI | Game w/ custom UI. |
| `send-user-message.ts` | pi‑mono examples | file | command | env | UI‑centric | Send user messages from extension. |
| `timed-confirm.ts` | pi‑mono examples | file | UI | env | UI‑centric | Abortable confirm/select dialogs. |
| `rpc-demo.ts` | pi‑mono examples | file | UI + RPC | env | UI‑centric | Exercises RPC UI methods. |
| `modal-editor.ts` | pi‑mono examples | file | UI | env | UI‑centric | Custom modal editor. |
| `rainbow-editor.ts` | pi‑mono examples | file | UI | env | UI‑centric | Animated editor content. |
| `notify.ts` | pi‑mono examples | file | UI | exec | OS‑heavy | Desktop notifications via OSC. |
| `titlebar-spinner.ts` | pi‑mono examples | file | UI | env | UI‑centric | Titlebar spinner animation. |
| `summarize.ts` | pi‑mono examples | file | command + tool | http | network‑heavy | Summarize with model call. |
| `custom-footer.ts` | pi‑mono examples | file | UI | env | UI‑centric | Footer customization. |
| `custom-header.ts` | pi‑mono examples | file | UI | env | UI‑centric | Header customization. |
| `overlay-test.ts` | pi‑mono examples | file | UI | env | UI‑centric | Overlay compositing tests. |
| `overlay-qa-tests.ts` | pi‑mono examples | file | UI | env | UI‑centric | Overlay QA suite. |
| `doom-overlay/` | pi‑mono examples | dir | UI | exec? | CPU/UI | Doom overlay @ 35 FPS. |
| `shutdown-command.ts` | pi‑mono examples | file | command | env | UI‑centric | `/quit` via `ctx.shutdown()`. |
| `interactive-shell.ts` | pi‑mono examples | file | event hook | exec | OS‑heavy | Interactive commands. |
| `inline-bash.ts` | pi‑mono examples | file | input transform | exec | OS‑heavy | `!{command}` expansion. |
| `bash-spawn-hook.ts` | pi‑mono examples | file | event hook | exec | OS‑heavy | Spawn hook for bash. |
| `input-transform.ts` | pi‑mono examples | file | event hook | env | UI‑centric | Input transformation. |
| `system-prompt-header.ts` | pi‑mono examples | file | prompt | env | UI‑centric | System prompt header. |

**Git Integration**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `git-checkpoint.ts` | pi‑mono examples | file | event hook | exec | FS‑heavy | Git stash checkpoints. |
| `auto-commit-on-exit.ts` | pi‑mono examples | file | lifecycle hook | exec | FS‑heavy | Auto‑commit on exit. |

**System Prompt & Compaction**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `pirate.ts` | pi‑mono examples | file | prompt | env | UI‑centric | `systemPromptAppend`. |
| `claude-rules.ts` | pi‑mono examples | file | prompt | read | FS‑heavy | Read `.claude/rules/`. |
| `custom-compaction.ts` | pi‑mono examples | file | compaction hook | env | UI‑centric | Custom compaction. |
| `trigger-compact.ts` | pi‑mono examples | file | command | env | UI‑centric | Trigger compaction on size. |

**System Integration / Resources / Messaging**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `mac-system-theme.ts` | pi‑mono examples | file | system integration | env | OS‑heavy | Sync theme with macOS. |
| `dynamic-resources/` | pi‑mono examples | dir | resource hook | read | FS‑heavy | `resources_discover`. |
| `message-renderer.ts` | pi‑mono examples | file | UI | env | UI‑centric | Custom message renderer. |
| `event-bus.ts` | pi‑mono examples | file | event hook | env | UI‑centric | Inter‑extension events. |
| `session-name.ts` | pi‑mono examples | file | session hook | env | UI‑centric | Set session name. |
| `bookmark.ts` | pi‑mono examples | file | session hook | env | UI‑centric | Bookmark entries. |

**Custom Providers**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `custom-provider-anthropic/` | pi‑mono examples | dir | provider | http | network‑heavy | Custom provider w/ OAuth. |
| `custom-provider-gitlab-duo/` | pi‑mono examples | dir | provider | http | network‑heavy | Provider via proxy. |
| `custom-provider-qwen-cli/` | pi‑mono examples | dir | provider | exec/http | network‑heavy | Qwen CLI provider. |

**External Dependencies**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `with-deps/` | pi‑mono examples | dir | mixed | read/write | FS‑heavy | Package.json + deps. |
| `file-trigger.ts` | pi‑mono examples | file | event hook | read | FS‑heavy | Watches trigger file. |

---

## B) GitHub Gists (badlogic)

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `diff.ts` | https://gist.github.com/badlogic/679b221a1749353a5be3f3134c120685 | gist | command + UI | exec | FS‑heavy | `/diff` command w/ UI; last active 2026‑01‑23. |
| `review-extension-v3.ts` | https://gist.github.com/badlogic/30aef35d686483ffce22cc2aad99f3ff | gist | command + session ops | write | FS‑heavy | `/review` branch‑from‑root; created 2026‑01‑16; other versions exist (v2/v1/corrected). |

---

## B2) Community GitHub Gists

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `terminal-title.ts` | https://gist.github.com/nicobailon/ee8a65353b9103ad5d149e7eeb452b10 | gist | event hook + UI | env | UI‑centric | Terminal tab title/status extension; created 2026‑01‑15. |
| `claude-style.ts` | https://gist.github.com/aadishv/7615082df075519d6efd9de793aa860a | gist | UI | env | UI‑centric | Claude‑style UI tweaks; created 2026‑01‑25. |

---

## C) Repo-local `.pi/extensions` (legacy pi-mono)

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `.pi/extensions/diff.ts` | pi‑mono `.pi` | file | command + UI | exec | FS‑heavy | Local diff UI extension. |
| `.pi/extensions/files.ts` | pi‑mono `.pi` | file | command + UI | read | FS‑heavy | File browser helper. |
| `.pi/extensions/prompt-url-widget.ts` | pi‑mono `.pi` | file | UI | http | network‑heavy | URL preview widget. |
| `.pi/extensions/redraws.ts` | pi‑mono `.pi` | file | UI | env | UI‑centric | UI redraw debugging. |

---

## D) Community / npm / Git Packages

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `agentsbox` | npm (agentsbox) | npm pkg | tool + MCP bridge | exec/http | network‑heavy | Installs a pi extension via `agentsbox setup pi`. |
| `pi-doom` | buildwithpi example | git pkg | UI overlay | exec | CPU/UI | Example git package install for pi (from official docs). |

---

## E) Notes & Next Steps

1. **Static capability scan**: parse each candidate to extract exact hostcall usage.  
2. **Enrich metadata**: add package.json name/version where present.  
3. **Sampling matrix**: use this list as input for `bd-22h` stratified selection.  
