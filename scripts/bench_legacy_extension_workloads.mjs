// Legacy (pi-mono) extension performance harness.
//
// Runs a few in-process scenarios against the pinned legacy `pi-mono` repo
// under `legacy_pi_mono_code/` and prints JSONL metrics to stdout.
//
// IMPORTANT: This script must be run with real Node.js, not Bun's `node` shim.
// Example:
//   /home/ubuntu/.nvm/versions/node/v22.2.0/bin/node scripts/bench_legacy_extension_workloads.mjs
//
// Output is JSONL so callers can append/aggregate deterministically.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { discoverAndLoadExtensions } from "../legacy_pi_mono_code/pi-mono/packages/coding-agent/dist/core/extensions/loader.js";
import { ExtensionRunner } from "../legacy_pi_mono_code/pi-mono/packages/coding-agent/dist/core/extensions/runner.js";
import { wrapRegisteredTools } from "../legacy_pi_mono_code/pi-mono/packages/coding-agent/dist/core/extensions/wrapper.js";
import { AuthStorage } from "../legacy_pi_mono_code/pi-mono/packages/coding-agent/dist/core/auth-storage.js";
import { ModelRegistry } from "../legacy_pi_mono_code/pi-mono/packages/coding-agent/dist/core/model-registry.js";
import { SessionManager } from "../legacy_pi_mono_code/pi-mono/packages/coding-agent/dist/core/session-manager.js";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const ROOT = path.resolve(__dirname, "..");

function nowNs() {
	return process.hrtime.bigint();
}

function nsToMs(ns) {
	return Number(ns) / 1_000_000;
}

function nsToUs(ns) {
	return Number(ns) / 1_000;
}

function mkdirTemp(prefix) {
	return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function percentile(sortedNumbers, pct) {
	if (sortedNumbers.length === 0) return null;
	if (pct <= 0) return sortedNumbers[0];
	if (pct >= 100) return sortedNumbers[sortedNumbers.length - 1];

	const idx = (pct / 100) * (sortedNumbers.length - 1);
	const lo = Math.floor(idx);
	const hi = Math.ceil(idx);
	if (lo === hi) return sortedNumbers[lo];
	const w = idx - lo;
	return sortedNumbers[lo] * (1 - w) + sortedNumbers[hi] * w;
}

function summarizeNs(valuesNs) {
	if (valuesNs.length === 0) {
		return {
			count: 0,
			min_ms: null,
			p50_ms: null,
			p95_ms: null,
			p99_ms: null,
			max_ms: null,
		};
	}

	const ms = valuesNs.map(nsToMs).sort((a, b) => a - b);
	return {
		count: ms.length,
		min_ms: ms[0],
		p50_ms: percentile(ms, 50),
		p95_ms: percentile(ms, 95),
		p99_ms: percentile(ms, 99),
		max_ms: ms[ms.length - 1],
	};
}

function noopBindings() {
	// Keep these minimal; most benchmarks only need tool execution and event dispatch.
	const actions = {
		sendMessage: () => {},
		sendUserMessage: () => {},
		appendEntry: () => {},
		setSessionName: () => {},
		getSessionName: () => undefined,
		setLabel: () => {},
		getActiveTools: () => [],
		getAllTools: () => [],
		setActiveTools: () => {},
		setModel: async () => {},
		getThinkingLevel: () => "off",
		setThinkingLevel: async () => {},
	};

	const contextActions = {
		getModel: () => undefined,
		isIdle: () => true,
		abort: () => {},
		hasPendingMessages: () => false,
		shutdown: () => {},
		getContextUsage: () => undefined,
		compact: () => {},
		getSystemPrompt: () => "",
	};

	return { actions, contextActions };
}

async function loadRunner(entryPath, cwd) {
	// Override agentDir to avoid accidentally loading user-installed extensions.
	const agentDir = mkdirTemp("pi-legacy-bench-agent-");
	const result = await discoverAndLoadExtensions([entryPath], cwd, agentDir);

	const sessionManager = SessionManager.inMemory();
	const authStorage = new AuthStorage(path.join(agentDir, "auth.json"));
	const modelRegistry = new ModelRegistry(authStorage);

	const runner = new ExtensionRunner(result.extensions, result.runtime, cwd, sessionManager, modelRegistry);
	const { actions, contextActions } = noopBindings();
	runner.bindCore(actions, contextActions);
	return runner;
}

async function scenarioLoadInitCold(extName, entryPath, { cwd, runs }) {
	const timingsNs = [];

	for (let i = 0; i < runs; i++) {
		const agentDir = mkdirTemp("pi-legacy-bench-agent-");

		const start = nowNs();
		const result = await discoverAndLoadExtensions([entryPath], cwd, agentDir);
		const sessionManager = SessionManager.inMemory();
		const authStorage = new AuthStorage(path.join(agentDir, "auth.json"));
		const modelRegistry = new ModelRegistry(authStorage);
		const runner = new ExtensionRunner(result.extensions, result.runtime, cwd, sessionManager, modelRegistry);
		const { actions, contextActions } = noopBindings();
		runner.bindCore(actions, contextActions);
		// Touch the registries so "load+init" includes basic access/validation.
		runner.getAllRegisteredTools();
		const end = nowNs();
		timingsNs.push(end - start);
	}

	return {
		schema: "pi.ext.legacy_bench.v1",
		runtime: "legacy_pi_mono",
		scenario: "ext_load_init/load_init_cold",
		extension: extName,
		runs,
		summary: summarizeNs(timingsNs),
		node: {
			version: process.version,
			platform: process.platform,
			arch: process.arch,
		},
	};
}

async function scenarioToolCall(extName, entryPath, toolName, toolInput, { cwd, iterations }) {
	const runner = await loadRunner(entryPath, cwd);
	const registered = runner.getAllRegisteredTools();
	const tools = wrapRegisteredTools(registered, runner);
	const tool = tools.find((t) => t.name === toolName);
	if (!tool) {
		throw new Error(`Tool not found: ${toolName} (extension=${extName})`);
	}

	const start = nowNs();
	for (let i = 0; i < iterations; i++) {
		// Keep callId stable; this mirrors the Rust benchmark and avoids extra allocations.
		// The tool interface treats it as an opaque identifier.
		// eslint-disable-next-line no-await-in-loop
		await tool.execute("bench-call-1", toolInput);
	}
	const elapsedNs = nowNs() - start;

	const perCallUs = nsToUs(elapsedNs) / iterations;
	const callsPerSec = (iterations * 1_000_000) / nsToUs(elapsedNs);

	return {
		schema: "pi.ext.legacy_bench.v1",
		runtime: "legacy_pi_mono",
		scenario: `ext_tool_call/${toolName}`,
		extension: extName,
		iterations,
		elapsed_ms: nsToMs(elapsedNs),
		per_call_us: perCallUs,
		calls_per_sec: callsPerSec,
		node: {
			version: process.version,
			platform: process.platform,
			arch: process.arch,
		},
	};
}

async function scenarioEventHook(extName, entryPath, { cwd, iterations }) {
	const runner = await loadRunner(entryPath, cwd);

	const start = nowNs();
	for (let i = 0; i < iterations; i++) {
		// eslint-disable-next-line no-await-in-loop
		await runner.emit({
			type: "before_agent_start",
			prompt: "",
			systemPrompt: "You are Pi.",
		});
	}
	const elapsedNs = nowNs() - start;

	const perCallUs = nsToUs(elapsedNs) / iterations;
	const callsPerSec = (iterations * 1_000_000) / nsToUs(elapsedNs);

	return {
		schema: "pi.ext.legacy_bench.v1",
		runtime: "legacy_pi_mono",
		scenario: "ext_event_hook/before_agent_start",
		extension: extName,
		iterations,
		elapsed_ms: nsToMs(elapsedNs),
		per_call_us: perCallUs,
		calls_per_sec: callsPerSec,
		node: {
			version: process.version,
			platform: process.platform,
			arch: process.arch,
		},
	};
}

function parseArgs(argv) {
	const args = {
		cwd: ROOT,
		loadRuns: Number(process.env.LOAD_RUNS ?? "5"),
		iterations: Number(process.env.ITERATIONS ?? "2000"),
		out: process.env.JSONL_OUT ?? null,
	};

	for (let i = 0; i < argv.length; i++) {
		const token = argv[i];
		if (token === "--load-runs") {
			args.loadRuns = Number(argv[++i] ?? "");
			continue;
		}
		if (token === "--iterations") {
			args.iterations = Number(argv[++i] ?? "");
			continue;
		}
		if (token === "--out") {
			args.out = argv[++i] ?? null;
			continue;
		}
		if (token === "--help" || token === "-h") {
			args.help = true;
			continue;
		}
		throw new Error(`Unknown arg: ${token}`);
	}

	if (!Number.isFinite(args.loadRuns) || args.loadRuns <= 0) {
		throw new Error(`--load-runs must be > 0 (got ${args.loadRuns})`);
	}
	if (!Number.isFinite(args.iterations) || args.iterations <= 0) {
		throw new Error(`--iterations must be > 0 (got ${args.iterations})`);
	}

	return args;
}

function usage() {
	return `Usage:
  node scripts/bench_legacy_extension_workloads.mjs [--load-runs N] [--iterations N] [--out PATH]

Env:
  LOAD_RUNS     default 5
  ITERATIONS    default 2000
  JSONL_OUT     if set, writes JSONL to this file (overwrites)

Example:
  /home/ubuntu/.nvm/versions/node/v22.2.0/bin/node scripts/bench_legacy_extension_workloads.mjs --iterations 5000 --out target/perf/legacy_extension_workloads.jsonl
`;
}

function openOut(outPath) {
	if (!outPath) return { writeLine: (line) => process.stdout.write(`${line}\n`) };

	const resolved = path.resolve(outPath);
	const parent = path.dirname(resolved);
	if (parent && parent !== "." && !fs.existsSync(parent)) {
		fs.mkdirSync(parent, { recursive: true });
	}
	// Deterministic: overwrite file each run.
	fs.writeFileSync(resolved, "");
	return {
		writeLine: (line) => fs.appendFileSync(resolved, `${line}\n`),
	};
}

async function main() {
	const args = parseArgs(process.argv.slice(2));
	if (args.help) {
		process.stdout.write(usage());
		return;
	}

	const out = openOut(args.out);

	const helloEntry = path.join(ROOT, "tests", "ext_conformance", "artifacts", "hello", "hello.ts");
	const pirateEntry = path.join(ROOT, "tests", "ext_conformance", "artifacts", "pirate", "pirate.ts");

	const results = [];
	results.push(await scenarioLoadInitCold("hello", helloEntry, { cwd: args.cwd, runs: args.loadRuns }));
	results.push(await scenarioLoadInitCold("pirate", pirateEntry, { cwd: args.cwd, runs: args.loadRuns }));
	results.push(
		await scenarioToolCall(
			"hello",
			helloEntry,
			"hello",
			{ name: "World" },
			{ cwd: args.cwd, iterations: args.iterations },
		),
	);
	results.push(await scenarioEventHook("pirate", pirateEntry, { cwd: args.cwd, iterations: args.iterations }));

	for (const row of results) {
		out.writeLine(JSON.stringify(row));
	}
}

await main();

