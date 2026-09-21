#!/usr/bin/env node

// Model-catalog generator for pi_agent_rust (gh #117 — optional "source the
// catalog from the upstream generated @earendil-works/pi-ai artifact" path).
//
// WHAT
//   Reads the upstream generated catalog (the baked `MODELS` export shipped in
//   @earendil-works/pi-ai's dist/models.generated.js) and re-emits it as the
//   exact `export const MODELS = {...} as const;` TypeScript that
//   src/models.rs::parse_legacy_generated_models() already accepts. Each model
//   keeps its `} satisfies Model<"<api>">,` clause (the Rust parser's
//   SATISFIES_RE strips those at parse time). The Rust parser is NOT changed.
//
// LOCAL OVERLAY (never regress pi-local entries)
//   pi-local-only entries and pi-curated overrides of upstream entries live in
//   scripts/models.local.json and are layered on top with LOCAL PRECEDENCE, so
//   a regenerate can never drop or mangle them. Today that overlay holds:
//     - the gh #115 z.ai GLM-5.1 / GLM-5.2 entries (curated cost + coding
//       endpoint; upstream ships them at 0 cost) and minimax / minimax-cn
//       MiniMax-M3 (curated cost + 1M context; upstream ships 512k @ different
//       cost), and
//     - the pi-only google-antigravity / google-gemini-cli native-adapter
//       providers, which upstream does not carry at all.
//
// USAGE
//   npm --prefix scripts ci                         # install the pinned deps
//   node scripts/gen-model-catalog.mjs              # -> scripts/models.generated.candidate.ts (SAFE default)
//   node scripts/gen-model-catalog.mjs --out FILE   # write the TS to FILE
//   node scripts/gen-model-catalog.mjs --check FILE # exit 1 if FILE != generated output (drift detection)
//
// The default output is a CANDIDATE file and does NOT touch the vendored
// catalog. See the "_README" block in scripts/models.local.json for why the
// vendored catalog is not (yet) regenerated from this upstream: the only
// published @earendil-works/pi-ai line (0.74.0+) is ~23-30 minor versions ahead
// of the vendored 0.51-era catalog (999 vs 687 models, +15 providers, and it
// omits the pi-local google-* providers), so an in-place overwrite would be a
// large, separately-warranted catalog upgrade rather than a safe drop-in.
//
// No runtime network access and not invoked from build.rs — this is a
// manual / CI generator only.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve as resolvePath } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const UPSTREAM_PKG = "@earendil-works/pi-ai";
const LOCAL_OVERLAY = resolvePath(HERE, "models.local.json");
const DEFAULT_OUT = resolvePath(HERE, "models.generated.candidate.ts");

// Field order matches the vendored catalog's per-model object layout exactly.
const MODEL_FIELD_ORDER = [
  "id",
  "name",
  "api",
  "provider",
  "baseUrl",
  "compat",
  "reasoning",
  "input",
  "cost",
  "contextWindow",
  "maxTokens",
  "headers",
];
const COST_FIELD_ORDER = ["input", "output", "cacheRead", "cacheWrite"];

function parseArgs(argv) {
  const args = { out: null, check: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--out") args.out = argv[++i];
    else if (a === "--check") args.check = argv[++i];
    else if (a === "-h" || a === "--help") args.help = true;
    else throw new Error(`Unknown argument: ${a}`);
  }
  return args;
}

function isPlainObject(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

// Deep merge with `over` (local) winning over `base` (upstream).
function deepMerge(base, over) {
  if (!isPlainObject(base) || !isPlainObject(over)) return structuredClone(over);
  const out = structuredClone(base);
  for (const [k, v] of Object.entries(over)) {
    out[k] = isPlainObject(v) && isPlainObject(out[k]) ? deepMerge(out[k], v) : structuredClone(v);
  }
  return out;
}

function resolveDistDir() {
  // Standard module resolution (real Node honors the "." export -> dist/index.js).
  try {
    return dirname(require.resolve(UPSTREAM_PKG));
  } catch {
    /* fall through */
  }
  // Fallback: the local node_modules layout (portable across Node/Bun even when
  // the package's `exports` map blocks bare specifier resolution).
  const guess = resolvePath(HERE, "node_modules", ...UPSTREAM_PKG.split("/"), "dist");
  if (existsSync(guess)) return guess;
  throw new Error(`Cannot locate ${UPSTREAM_PKG}; run 'npm --prefix scripts ci' first`);
}

async function loadUpstreamCatalog() {
  // The catalog ships as a baked artifact. The package's runtime API
  // (createModels()/getModels()) is instance-based and needs provider
  // registration; the baked `MODELS` export is the deterministic source.
  const distDir = resolveDistDir();
  const artifact = resolvePath(distDir, "models.generated.js");
  const mod = await import(pathToFileURL(artifact).href);
  if (!mod.MODELS) throw new Error(`${UPSTREAM_PKG} did not export MODELS from ${artifact}`);
  let version = "unknown";
  try {
    version = JSON.parse(readFileSync(resolvePath(distDir, "..", "package.json"), "utf8")).version;
  } catch {
    /* best effort */
  }
  return { models: mod.MODELS, version };
}

function loadLocalOverlay() {
  let raw;
  try {
    raw = readFileSync(LOCAL_OVERLAY, "utf8");
  } catch {
    return {};
  }
  const parsed = JSON.parse(raw);
  // Accept either { providers: {...} } or a bare { provider: {...} } map.
  const providers = parsed.providers ?? parsed;
  const out = {};
  for (const [k, v] of Object.entries(providers)) {
    if (k.startsWith("_")) continue; // skip _README / _comment keys
    out[k] = v;
  }
  return out;
}

function mergeCatalogs(upstream, local) {
  const out = structuredClone(upstream);
  for (const [provider, models] of Object.entries(local)) {
    out[provider] ??= {};
    for (const [id, model] of Object.entries(models)) {
      out[provider][id] = deepMerge(out[provider][id] ?? {}, model);
    }
  }
  return out;
}

function fmtNum(n) {
  if (typeof n !== "number" || !Number.isFinite(n)) return "0";
  // Avoid scientific notation for the small magnitudes used by costs.
  if (Number.isInteger(n)) return String(n);
  return n.toFixed(12).replace(/0+$/, "").replace(/\.$/, "");
}

function emitModel(model, tab) {
  const t = tab;
  const lines = [];
  for (const field of MODEL_FIELD_ORDER) {
    if (!(field in model)) continue;
    const v = model[field];
    switch (field) {
      case "id":
      case "name":
      case "api":
      case "provider":
      case "baseUrl":
        lines.push(`${t}\t${field}: ${JSON.stringify(String(v))},`);
        break;
      case "reasoning":
        lines.push(`${t}\t${field}: ${v ? "true" : "false"},`);
        break;
      case "input":
        lines.push(`${t}\tinput: [${(v ?? []).map((x) => JSON.stringify(String(x))).join(", ")}],`);
        break;
      case "contextWindow":
      case "maxTokens":
        lines.push(`${t}\t${field}: ${fmtNum(v)},`);
        break;
      case "cost": {
        const c = v ?? {};
        lines.push(`${t}\tcost: {`);
        for (const ck of COST_FIELD_ORDER) lines.push(`${t}\t\t${ck}: ${fmtNum(c[ck] ?? 0)},`);
        lines.push(`${t}\t},`);
        break;
      }
      case "compat":
      case "headers":
        if (v && (typeof v !== "object" || Object.keys(v).length > 0)) {
          lines.push(`${t}\t${field}: ${JSON.stringify(v)},`);
        }
        break;
      default:
        break;
    }
  }
  return lines;
}

function emitCatalog(merged, version) {
  const out = [];
  out.push("// This file is auto-generated by scripts/gen-model-catalog.mjs");
  out.push(
    `// Source: ${UPSTREAM_PKG}@${version} (upstream MODELS artifact) + scripts/models.local.json overlay`,
  );
  out.push(
    "// Do not edit manually - run 'node scripts/gen-model-catalog.mjs --out <this file>' to update",
  );
  out.push("");
  out.push('import type { Model } from "./types.js";');
  out.push("");
  out.push("export const MODELS = {");

  const providers = Object.keys(merged).sort((a, b) => a.localeCompare(b));
  for (const provider of providers) {
    out.push(`\t${JSON.stringify(provider)}: {`);
    const ids = Object.keys(merged[provider]).sort((a, b) => a.localeCompare(b));
    for (const id of ids) {
      const model = merged[provider][id];
      out.push(`\t\t${JSON.stringify(id)}: {`);
      out.push(...emitModel(model, "\t\t"));
      const api = String(model.api ?? "");
      out.push(`\t\t} satisfies Model<${JSON.stringify(api)}>,`);
    }
    out.push("\t},");
  }
  out.push("} as const;");
  out.push("");
  return out.join("\n");
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(
      readFileSync(fileURLToPath(import.meta.url), "utf8")
        .split("\n")
        .filter((l) => l.startsWith("//"))
        .join("\n") + "\n",
    );
    return;
  }

  const { models: upstream, version } = await loadUpstreamCatalog();
  const local = loadLocalOverlay();
  const merged = mergeCatalogs(upstream, local);

  const upstreamProviders = Object.keys(upstream).length;
  const upstreamModels = Object.values(upstream).reduce((n, m) => n + Object.keys(m).length, 0);
  const localProviders = Object.keys(local).length;
  const localModels = Object.values(local).reduce((n, m) => n + Object.keys(m).length, 0);
  const mergedProviders = Object.keys(merged).length;
  const mergedModels = Object.values(merged).reduce((n, m) => n + Object.keys(m).length, 0);

  const ts = emitCatalog(merged, version);

  process.stderr.write(
    `[gen-model-catalog] upstream ${UPSTREAM_PKG}@${version}: ${upstreamProviders} providers / ${upstreamModels} models\n` +
      `[gen-model-catalog] local overlay: ${localProviders} providers / ${localModels} models\n` +
      `[gen-model-catalog] merged (upstream u local): ${mergedProviders} providers / ${mergedModels} models\n`,
  );

  if (args.check !== null) {
    let current = "";
    try {
      current = readFileSync(args.check, "utf8");
    } catch {
      process.stderr.write(`[gen-model-catalog] --check target not found: ${args.check}\n`);
      process.exit(1);
    }
    if (current === ts) {
      process.stderr.write(`[gen-model-catalog] in sync: ${args.check}\n`);
      process.exit(0);
    }
    process.stderr.write(
      `[gen-model-catalog] DRIFT: ${args.check} differs from generated output\n`,
    );
    process.exit(1);
  }

  const outPath = args.out ? resolvePath(process.cwd(), args.out) : DEFAULT_OUT;
  writeFileSync(outPath, ts);
  process.stderr.write(`[gen-model-catalog] wrote ${outPath}\n`);
}

main().catch((err) => {
  process.stderr.write(`[gen-model-catalog] ERROR: ${err?.stack || err}\n`);
  process.exit(1);
});
