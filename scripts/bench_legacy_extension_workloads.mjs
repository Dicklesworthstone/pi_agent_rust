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
import { fileURLToPath, pathToFileURL } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const ROOT = path.resolve(__dirname, "..");
const RUNTIME_KIND = process.versions.bun ? "bun" : "node";
const RUNTIME_LABEL = `portable_${RUNTIME_KIND}_extension_api`;
const CORRELATION_ID = process.env.CI_CORRELATION_ID?.trim() || null;
const SOURCE_COMMIT = process.env.VERGEN_GIT_SHA?.trim() || null;
const SOURCE_DIRTY = process.env.VERGEN_GIT_DIRTY !== "false";
const GENERATED_AT = new Date().toISOString();
const EVIDENCE_PROVENANCE = Object.freeze({
	timestamp: GENERATED_AT,
	run_id: CORRELATION_ID,
	correlation_id: CORRELATION_ID,
	source_commit: SOURCE_COMMIT,
	source_dirty: SOURCE_DIRTY,
});
const COMPILED_MODULES = new Map();
const COMPILED_SOURCES = new Map();

const TYPEBOX_STUB = `const Type = {
  Object: (properties = {}, options = {}) => ({ type: "object", properties, ...options }),
  String: (options = {}) => ({ type: "string", ...options }),
  Number: (options = {}) => ({ type: "number", ...options }),
  Boolean: (options = {}) => ({ type: "boolean", ...options }),
  Array: (items, options = {}) => ({ type: "array", items, ...options }),
  Optional: (schema) => ({ ...schema, optional: true }),
  Literal: (value) => ({ const: value }),
  Union: (anyOf, options = {}) => ({ anyOf, ...options }),
};`;

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

function runtimeMetadata() {
	return {
		kind: RUNTIME_KIND,
		version: process.versions.bun ?? process.version,
		platform: process.platform,
		arch: process.arch,
	};
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

function transpileFixtureSource(source) {
	return source
		.replace(/^\s*import\s+type\s+[^;]+;\s*$/gm, "")
		.replace(
			/^\s*import\s+\{\s*Type\s*\}\s+from\s+["']@sinclair\/typebox["'];\s*$/gm,
			TYPEBOX_STUB,
		)
		.replace(/:\s*ExtensionAPI\b/g, "")
		.replace(/\s+as\s+\{[^}]*\}/g, "");
}

function compiledSource(entryPath) {
	const resolved = path.resolve(entryPath);
	const cached = COMPILED_SOURCES.get(resolved);
	if (cached) return cached;

	const source = fs.readFileSync(resolved, "utf-8");
	const compiled = transpileFixtureSource(source);
	COMPILED_SOURCES.set(resolved, compiled);
	return compiled;
}

function sanitizeNonce(nonce) {
	return String(nonce).replace(/[^A-Za-z0-9_.-]/g, "_");
}

function compiledModulePath(entryPath) {
	const resolved = path.resolve(entryPath);
	const cached = COMPILED_MODULES.get(resolved);
	if (cached) return cached;

	const compiled = compiledSource(resolved);
	const tempDir = mkdirTemp("pi-legacy-portable-extension-");
	const outPath = path.join(tempDir, `${path.basename(resolved).replace(/\.[^.]+$/, "")}.mjs`);
	fs.writeFileSync(outPath, compiled, "utf-8");
	COMPILED_MODULES.set(resolved, outPath);
	return outPath;
}

async function loadExtensionFactory(entryPath, nonce) {
	let compiledPath = compiledModulePath(entryPath);
	if (RUNTIME_KIND === "bun") {
		// Bun 1.3 caches file imports even when a query string changes. Use a
		// distinct temporary module file for each cache-busted load so cold-load
		// and full-session measurements actually reparse the extension source.
		const resolved = path.resolve(entryPath);
		const tempDir = mkdirTemp("pi-legacy-portable-bun-extension-");
		compiledPath = path.join(
			tempDir,
			`${path.basename(resolved).replace(/\.[^.]+$/, "")}-${sanitizeNonce(nonce)}.mjs`,
		);
		fs.writeFileSync(compiledPath, compiledSource(resolved), "utf-8");
	}
	const moduleUrl = `${pathToFileURL(compiledPath).href}?run=${encodeURIComponent(nonce)}`;
	const module = await import(moduleUrl);
	if (typeof module.default !== "function") {
		throw new Error(`Extension does not export a default factory: ${entryPath}`);
	}
	return module.default;
}

function createNoopContext(cwd) {
	return {
		ui: {
			select: async () => undefined,
			confirm: async () => false,
			input: async () => undefined,
			notify: () => {},
			setStatus: () => {},
			setWorkingMessage: () => {},
			setWidget: () => {},
			setFooter: () => {},
			setHeader: () => {},
			setTitle: () => {},
			custom: async () => undefined,
			setEditorText: () => {},
			getEditorText: () => "",
			editor: async () => undefined,
			setEditorComponent: () => {},
			getAllThemes: () => [],
			getTheme: () => undefined,
			setTheme: () => ({ success: false, error: "UI not available" }),
		},
		hasUI: false,
		cwd,
		sessionManager: {
			getCurrentSession: () => undefined,
			listSessions: () => [],
		},
		modelRegistry: {
			getCurrentModel: () => undefined,
			listModels: () => [],
		},
		get model() {
			return undefined;
		},
		isIdle: () => true,
		abort: () => {},
		hasPendingMessages: () => false,
		shutdown: () => {},
		getContextUsage: () => undefined,
		compact: () => {},
		getSystemPrompt: () => "",
	};
}

async function loadRunner(entryPath, cwd, nonce = `${Date.now()}-${Math.random()}`) {
	const tools = new Map();
	const commands = new Map();
	const handlers = new Map();
	const flags = new Map();
	const flagValues = new Map();

	const api = {
		on(event, handler) {
			const list = handlers.get(event) ?? [];
			list.push(handler);
			handlers.set(event, list);
		},
		registerTool(tool) {
			tools.set(tool.name, tool);
		},
		registerCommand(name, options) {
			commands.set(name, { name, ...options });
		},
		registerShortcut() {},
		registerFlag(name, options) {
			flags.set(name, options);
			if (Object.hasOwn(options, "default")) {
				flagValues.set(name, options.default);
			}
		},
		registerMessageRenderer() {},
		getFlag(name) {
			return flagValues.get(name);
		},
		sendMessage() {},
		sendUserMessage() {},
		appendEntry() {},
		setSessionName() {},
		getSessionName: () => undefined,
		setLabel() {},
		exec: async () => {
			throw new Error("portable benchmark harness does not execute shell commands");
		},
		getActiveTools: () => [...tools.keys()],
		getAllTools: () => [...tools.values()],
		setActiveTools() {},
		setModel: async () => {},
		getThinkingLevel: () => "off",
		setThinkingLevel: async () => {},
		registerProvider() {},
		events: {
			on: () => {},
			emit: async () => {},
		},
	};

	const factory = await loadExtensionFactory(entryPath, nonce);
	await factory(api);

	return {
		tools,
		commands,
		handlers,
		createContext: () => createNoopContext(cwd),
		hasHandlers: (eventName) => (handlers.get(eventName) ?? []).length > 0,
		async emit(event) {
			let result;
			const ctx = createNoopContext(cwd);
			for (const handler of handlers.get(event.type) ?? []) {
				const handlerResult = await handler(event, ctx);
				if (handlerResult !== undefined) {
					result = handlerResult;
				}
			}
			return result;
		},
	};
}

async function scenarioLoadInitCold(extName, entryPath, { cwd, runs }) {
	const timingsNs = [];

	for (let i = 0; i < runs; i++) {
		const start = nowNs();
		const runner = await loadRunner(entryPath, cwd, `cold-${extName}-${RUNTIME_KIND}-${i}`);
		// Touch the registries so "load+init" includes basic access/validation.
		if (runner.tools.size === 0 && runner.commands.size === 0 && runner.handlers.size === 0) {
			throw new Error(`Extension registered no tools, commands, or handlers: ${entryPath}`);
		}
		const end = nowNs();
		timingsNs.push(end - start);
	}

	return {
		schema: "pi.ext.legacy_bench.v1",
		...EVIDENCE_PROVENANCE,
		runtime: RUNTIME_LABEL,
		runtime_kind: RUNTIME_KIND,
		runtime_family: "portable_extension_api",
		scenario: "ext_load_init/load_init_cold",
		extension: extName,
		runs,
		summary: summarizeNs(timingsNs),
		node: {
			version: process.version,
			platform: process.platform,
			arch: process.arch,
		},
		runtime_metadata: runtimeMetadata(),
	};
}

async function scenarioToolCall(extName, entryPath, toolName, toolInput, { cwd, iterations }) {
	const runner = await loadRunner(entryPath, cwd);
	const tool = runner.tools.get(toolName);
	if (!tool) {
		throw new Error(`Tool not found: ${toolName} (extension=${extName})`);
	}

	const start = nowNs();
	for (let i = 0; i < iterations; i++) {
		// Keep callId stable; this mirrors the Rust benchmark and avoids extra allocations.
		// The tool interface treats it as an opaque identifier.
		// eslint-disable-next-line no-await-in-loop
		await tool.execute("bench-call-1", toolInput, undefined, () => {}, runner.createContext());
	}
	const elapsedNs = nowNs() - start;

	const perCallUs = nsToUs(elapsedNs) / iterations;
	const callsPerSec = (iterations * 1_000_000) / nsToUs(elapsedNs);

	return {
		schema: "pi.ext.legacy_bench.v1",
		...EVIDENCE_PROVENANCE,
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
		runtime: RUNTIME_LABEL,
		runtime_kind: RUNTIME_KIND,
		runtime_family: "portable_extension_api",
		runtime_metadata: runtimeMetadata(),
	};
}

async function scenarioEventHook(extName, entryPath, { cwd, iterations }) {
	const runner = await loadRunner(entryPath, cwd);
	const command = runner.commands.get("pirate");
	if (command?.handler) {
		await command.handler("", {
			...runner.createContext(),
			waitForIdle: async () => {},
			newSession: async () => ({ cancelled: false }),
			fork: async () => ({ cancelled: false }),
			navigateTree: async () => ({ cancelled: false }),
			switchSession: async () => ({ cancelled: false }),
		});
	}

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
		...EVIDENCE_PROVENANCE,
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
		runtime: RUNTIME_LABEL,
		runtime_kind: RUNTIME_KIND,
		runtime_family: "portable_extension_api",
		runtime_metadata: runtimeMetadata(),
	};
}

async function scenarioFullE2ELongSession(helloEntry, pirateEntry, { cwd, iterations, toolCalls }) {
	const sessionTurns = iterations;
	const start = nowNs();
	let toolExecutions = 0;
	let eventExecutions = 0;

	for (let i = 0; i < sessionTurns; i++) {
		const helloRunner = await loadRunner(helloEntry, cwd, `full-hello-${RUNTIME_KIND}-${i}`);
		const helloTool = helloRunner.tools.get("hello");
		if (!helloTool) {
			throw new Error("Tool not found during full E2E workload: hello");
		}
		for (let j = 0; j < toolCalls; j++) {
			// eslint-disable-next-line no-await-in-loop
			await helloTool.execute(
				`bench-call-${i}-${j}`,
				{ name: "World" },
				undefined,
				() => {},
				helloRunner.createContext(),
			);
			toolExecutions += 1;
		}

		const pirateRunner = await loadRunner(pirateEntry, cwd, `full-pirate-${RUNTIME_KIND}-${i}`);
		const command = pirateRunner.commands.get("pirate");
		if (command?.handler) {
			// eslint-disable-next-line no-await-in-loop
			await command.handler("", {
				...pirateRunner.createContext(),
				waitForIdle: async () => {},
				newSession: async () => ({ cancelled: false }),
				fork: async () => ({ cancelled: false }),
				navigateTree: async () => ({ cancelled: false }),
				switchSession: async () => ({ cancelled: false }),
			});
		}
		// eslint-disable-next-line no-await-in-loop
		await pirateRunner.emit({
			type: "before_agent_start",
			prompt: "",
			systemPrompt: "You are Pi.",
		});
		eventExecutions += 1;
	}

	const elapsedNs = nowNs() - start;
	const elapsedMs = nsToMs(elapsedNs);

	return {
		schema: "pi.ext.legacy_bench.v1",
		...EVIDENCE_PROVENANCE,
		runtime: RUNTIME_LABEL,
		runtime_kind: RUNTIME_KIND,
		runtime_family: "portable_extension_api",
		scenario: "full_e2e_long_session",
		extension: "hello+pirate",
		iterations: sessionTurns,
		tool_calls_per_iteration: toolCalls,
		tool_executions: toolExecutions,
		event_executions: eventExecutions,
		elapsed_ms: elapsedMs,
		per_iteration_ms: elapsedMs / sessionTurns,
		calls_per_sec: ((toolExecutions + eventExecutions) * 1_000_000) / nsToUs(elapsedNs),
		workload_shape: {
			description:
				"cache-busted extension load, hello tool dispatch, pirate command toggle, and before_agent_start event per session turn",
			extension_loads_per_iteration: 2,
			tool_calls_per_iteration: toolCalls,
			event_hooks_per_iteration: 1,
		},
		node: {
			version: process.version,
			platform: process.platform,
			arch: process.arch,
		},
		runtime_metadata: runtimeMetadata(),
	};
}

function parseArgs(argv) {
	const args = {
		cwd: ROOT,
		loadRuns: Number(process.env.LOAD_RUNS ?? "5"),
		iterations: Number(process.env.ITERATIONS ?? "2000"),
		toolCalls: Number(process.env.TOOL_CALLS ?? "10"),
		out: process.env.JSONL_OUT ?? null,
		append: false,
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
		if (token === "--tool-calls") {
			args.toolCalls = Number(argv[++i] ?? "");
			continue;
		}
		if (token === "--out") {
			args.out = argv[++i] ?? null;
			continue;
		}
		if (token === "--append") {
			args.append = true;
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
	if (!Number.isFinite(args.toolCalls) || args.toolCalls <= 0) {
		throw new Error(`--tool-calls must be > 0 (got ${args.toolCalls})`);
	}

	return args;
}

function usage() {
	return `Usage:
  node scripts/bench_legacy_extension_workloads.mjs [--load-runs N] [--iterations N] [--tool-calls N] [--out PATH] [--append]

Env:
  LOAD_RUNS     default 5
  ITERATIONS    default 2000
  TOOL_CALLS    default 10
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
	if (!globalThis.__PI_LEGACY_BENCH_APPEND) {
		// Deterministic by default: overwrite file each run.
		fs.writeFileSync(resolved, "");
	}
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

	globalThis.__PI_LEGACY_BENCH_APPEND = args.append;
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
	results.push(
		await scenarioFullE2ELongSession(helloEntry, pirateEntry, {
			cwd: args.cwd,
			iterations: args.iterations,
			toolCalls: args.toolCalls,
		}),
	);

	for (const row of results) {
		out.writeLine(JSON.stringify(row));
	}
}

await main();
