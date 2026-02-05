/**
 * Dynamic validation runner: loads every extension in VALIDATED_MANIFEST.json
 * through the pi-mono loader (via run_extension.ts) and writes a JSON report.
 *
 * Usage (from repo root):
 *   bun run tests/ext_conformance/ts_oracle/validate_manifest.ts
 *
 * Optional env:
 * - PI_TS_VALIDATE_LIMIT=N        Limit number of extensions (debug)
 * - PI_TS_VALIDATE_TIER=official-pi-mono|community|npm-registry|third-party-github|agents-mikeastock
 * - PI_TS_VALIDATE_TIMEOUT_MS=15000
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

interface ManifestEntry {
	id: string;
	entry_path: string;
	source_tier: string;
	mock_requirements?: string[];
}

interface ManifestFile {
	schema?: string;
	generated_at?: string;
	extensions: ManifestEntry[];
}

interface HarnessResult {
	success: boolean;
	error: string | null;
	load_time_ms: number | null;
	extension: {
		path: string;
		resolvedPath: string;
		handlers: Record<string, number>;
		tools: JsonValue[];
		commands: JsonValue[];
		shortcuts: JsonValue[];
		flags: JsonValue[];
		messageRenderers: string[];
		providers: JsonValue[];
		flagValues: Record<string, boolean | string>;
	} | null;
	runtime?: JsonValue;
	capture?: JsonValue;
	logs?: Array<{ level: string; message: string }>;
}

interface ValidationResult {
	id: string;
	entry_path: string;
	source_tier: string;
	mock_requirements: string[];
	abs_path: string;
	load_success: boolean;
	error: string | null;
	load_time_ms: number | null;
	harness_duration_ms: number;
	registrations: HarnessResult["extension"];
}

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "../../..");

const MANIFEST_PATH = path.join(REPO_ROOT, "tests/ext_conformance/VALIDATED_MANIFEST.json");
const ARTIFACTS_ROOT = path.join(REPO_ROOT, "tests/ext_conformance/artifacts");
const HARNESS_PATH = path.join(REPO_ROOT, "tests/ext_conformance/ts_harness/run_extension.ts");
const DEFAULT_MOCK_SPEC = path.join(REPO_ROOT, "tests/ext_conformance/mock_specs/mock_spec_default.json");
const OUTPUT_PATH = path.join(REPO_ROOT, "tests/ext_conformance/ts_oracle/dynamic_validation_results.json");
const PI_MONO_ROOT = path.join(REPO_ROOT, "legacy_pi_mono_code/pi-mono");

const BUN = process.env.BUN ?? "/home/ubuntu/.bun/bin/bun";
const TIMEOUT_MS = Number(process.env.PI_TS_VALIDATE_TIMEOUT_MS ?? "15000");
const LIMIT = Number(process.env.PI_TS_VALIDATE_LIMIT ?? "0");
const FILTER_TIER = process.env.PI_TS_VALIDATE_TIER;
const CWD = process.env.PI_TS_VALIDATE_CWD ?? REPO_ROOT;

function readManifest(filePath: string): ManifestFile {
	const raw = fs.readFileSync(filePath, "utf-8");
	return JSON.parse(raw) as ManifestFile;
}

function summarize(results: ValidationResult[]) {
	const byTier: Record<string, { total: number; passed: number; failed: number }> = {};
	for (const result of results) {
		const tier = result.source_tier;
		if (!byTier[tier]) {
			byTier[tier] = { total: 0, passed: 0, failed: 0 };
		}
		byTier[tier].total += 1;
		if (result.load_success) {
			byTier[tier].passed += 1;
		} else {
			byTier[tier].failed += 1;
		}
	}
	return {
		total: results.length,
		passed: results.filter((r) => r.load_success).length,
		failed: results.filter((r) => !r.load_success).length,
		by_tier: byTier,
	};
}

async function runHarness(entryAbs: string): Promise<{
	parsed: HarnessResult | null;
	error: string | null;
	durationMs: number;
	timedOut: boolean;
}> {
	const start = Date.now();
	const proc = Bun.spawn({
		cmd: [BUN, "run", HARNESS_PATH, entryAbs, DEFAULT_MOCK_SPEC, CWD],
		stdout: "pipe",
		stderr: "pipe",
		env: {
			...process.env,
			NODE_PATH: path.join(PI_MONO_ROOT, "node_modules"),
			PI_TS_CAPTURE_LOGS: "1",
		},
	});

	let timedOut = false;
	const timeout = setTimeout(() => {
		timedOut = true;
		try {
			proc.kill();
		} catch {
			// ignore
		}
	}, TIMEOUT_MS);

	const [stdout, stderr] = await Promise.all([
		new Response(proc.stdout).text(),
		new Response(proc.stderr).text(),
		proc.exited,
	]);
	clearTimeout(timeout);
	const durationMs = Date.now() - start;

	if (timedOut) {
		return { parsed: null, error: `timeout after ${TIMEOUT_MS}ms`, durationMs, timedOut };
	}

	const trimmed = stdout.trim();
	if (!trimmed) {
		return { parsed: null, error: `empty stdout (stderr: ${stderr.trim().slice(0, 200)})`, durationMs, timedOut };
	}

	try {
		const parsed = JSON.parse(trimmed) as HarnessResult;
		return { parsed, error: null, durationMs, timedOut };
	} catch (err) {
		const errMsg = err instanceof Error ? err.message : String(err);
		const snippet = trimmed.slice(0, 400);
		return {
			parsed: null,
			error: `json parse failed: ${errMsg}; stdout snippet: ${snippet}`,
			durationMs,
			timedOut,
		};
	}
}

async function main() {
	const manifest = readManifest(MANIFEST_PATH);
	let entries = manifest.extensions;
	if (FILTER_TIER) {
		entries = entries.filter((entry) => entry.source_tier === FILTER_TIER);
	}
	if (LIMIT > 0) {
		entries = entries.slice(0, LIMIT);
	}

	const results: ValidationResult[] = [];
	for (const entry of entries) {
		const absPath = path.resolve(ARTIFACTS_ROOT, entry.entry_path);
		if (!fs.existsSync(absPath)) {
			results.push({
				id: entry.id,
				entry_path: entry.entry_path,
				source_tier: entry.source_tier,
				mock_requirements: entry.mock_requirements ?? [],
				abs_path: absPath,
				load_success: false,
				error: "entry_path missing on disk",
				load_time_ms: null,
				harness_duration_ms: 0,
				registrations: null,
			});
			continue;
		}

		const { parsed, error, durationMs } = await runHarness(absPath);
		const loadSuccess = parsed?.success === true && !error;

		results.push({
			id: entry.id,
			entry_path: entry.entry_path,
			source_tier: entry.source_tier,
			mock_requirements: entry.mock_requirements ?? [],
			abs_path: absPath,
			load_success: loadSuccess,
			error: error ?? parsed?.error ?? null,
			load_time_ms: parsed?.load_time_ms ?? null,
			harness_duration_ms: durationMs,
			registrations: parsed?.extension ?? null,
		});
	}

	const summary = summarize(results);
	const output = {
		schema: "pi.ext.dynamic_validation.v1",
		generated_at: new Date().toISOString(),
		harness: {
			script: "tests/ext_conformance/ts_harness/run_extension.ts",
			mock_spec: "tests/ext_conformance/mock_specs/mock_spec_default.json",
			timeout_ms: TIMEOUT_MS,
			cwd: CWD,
		},
		manifest: {
			path: "tests/ext_conformance/VALIDATED_MANIFEST.json",
			schema: manifest.schema ?? null,
			generated_at: manifest.generated_at ?? null,
		},
		summary,
		results,
	};

	fs.writeFileSync(OUTPUT_PATH, JSON.stringify(output, null, 2));
	console.log(
		`Dynamic validation complete. total=${summary.total} passed=${summary.passed} failed=${summary.failed}`,
	);
	console.log(`Wrote ${OUTPUT_PATH}`);
}

main();
