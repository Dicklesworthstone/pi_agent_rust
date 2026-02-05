/**
 * TS Scenario Runner: load a pi-mono extension and execute a scenario
 * (tool call, command, or event dispatch), returning the execution result.
 *
 * Usage:
 *   echo '{"kind":"tool","tool_name":"hello","input":{}}' | \
 *     bun run tests/ext_conformance/ts_harness/run_scenario.ts <extension-path> <mock-spec-path>
 *
 * Input (stdin JSON):
 *   { kind: "tool"|"command"|"event"|"provider",
 *     tool_name?: string, command_name?: string, event_name?: string,
 *     input?: { arguments?: object, args?: string, event?: object, ctx?: object } }
 *
 * Output (stdout JSON):
 *   { success: boolean, error?: string, kind: string,
 *     result?: any, load_time_ms: number, exec_time_ms: number }
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

interface MockSpec {
	schema?: string;
	extension_id?: string;
	session?: {
		name?: string;
		state?: JsonValue;
		messages?: JsonValue[];
		entries?: JsonValue[];
		branch?: JsonValue[];
		accept_mutations?: boolean;
	};
	http?: { rules?: Array<{ method: string; url: string; response: { status: number; headers?: Record<string, string>; body?: string } }>; default_response?: { status: number; body?: string } };
	exec?: { rules?: Array<{ command: string; args?: string[]; result: { stdout: string; stderr: string; code: number; killed?: boolean } }>; default_result?: { stdout: string; stderr: string; code: number; killed?: boolean } };
	tools?: { active_tools?: string[]; all_tools?: Array<{ name: string; description?: string }> };
	ui?: { capture?: boolean; responses?: Record<string, JsonValue>; confirm_default?: boolean };
	model?: { current?: { provider?: string; model_id?: string; name?: string }; thinking_level?: string; accept_mutations?: boolean };
}

interface ScenarioInput {
	kind: string;
	tool_name?: string;
	command_name?: string;
	event_name?: string;
	input?: JsonValue;
}

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const PI_MONO_ROOT = path.resolve(__dirname, "../../../legacy_pi_mono_code/pi-mono");

const loaderPath = path.join(PI_MONO_ROOT, "packages/coding-agent/dist/core/extensions/loader.js");
const { loadExtensions } = await import(loaderPath);

const originalConsole = {
	log: console.log.bind(console),
	error: console.error.bind(console),
};

// Suppress extension console output
console.log = () => {};
console.warn = () => {};
console.error = () => {};

function applyDeterministicGlobals() {
	const timeRaw = process.env.PI_DETERMINISTIC_TIME_MS;
	const stepRaw = process.env.PI_DETERMINISTIC_TIME_STEP_MS;
	if (timeRaw && timeRaw.trim().length > 0) {
		const base = Number(timeRaw);
		if (Number.isFinite(base)) {
			const stepValue = stepRaw ? Number(stepRaw) : 1;
			const step = Number.isFinite(stepValue) ? stepValue : 1;
			let tick = 0;
			const nextNow = () => { const v = base + step * tick; tick += 1; return v; };
			const OriginalDate = Date;
			class DeterministicDate extends OriginalDate {
				constructor(...args: any[]) { if (args.length === 0) { super(nextNow()); } else { super(...args); } }
				static now() { return nextNow(); }
			}
			DeterministicDate.UTC = OriginalDate.UTC;
			DeterministicDate.parse = OriginalDate.parse;
			(globalThis as any).Date = DeterministicDate;
		}
	}
	const randRaw = process.env.PI_DETERMINISTIC_RANDOM;
	const randSeedRaw = process.env.PI_DETERMINISTIC_RANDOM_SEED;
	if (randRaw && randRaw.trim().length > 0) {
		const value = Number(randRaw);
		if (Number.isFinite(value)) { Math.random = () => value; }
	} else if (randSeedRaw && randSeedRaw.trim().length > 0) {
		let state = Number(randSeedRaw);
		if (Number.isFinite(state)) {
			state = state >>> 0;
			Math.random = () => { state = (state * 1664525 + 1013904223) >>> 0; return state / 4294967296; };
		}
	}
	const detCwd = process.env.PI_DETERMINISTIC_CWD;
	if (detCwd && detCwd.trim().length > 0) {
		try { Object.defineProperty(process, "cwd", { value: () => detCwd, configurable: true }); } catch {}
	}
	const detHome = process.env.PI_DETERMINISTIC_HOME;
	if (detHome && detHome.trim().length > 0) {
		try { process.env.HOME = detHome; process.env.USERPROFILE = detHome; } catch {}
	}
}

function readJson(filePath: string): JsonValue {
	return JSON.parse(fs.readFileSync(filePath, "utf-8")) as JsonValue;
}

function readStdin(): Promise<string> {
	return new Promise((resolve) => {
		let data = "";
		process.stdin.setEncoding("utf-8");
		process.stdin.on("data", (chunk: string) => { data += chunk; });
		process.stdin.on("end", () => resolve(data));
	});
}

function pickExecRule(rules: MockSpec["exec"]["rules"], command: string, args: string[]) {
	if (!rules) return undefined;
	return rules.find((rule) => {
		if (rule.command !== command) return false;
		if (!rule.args) return true;
		return rule.args.length === args.length && rule.args.every((v, i) => v === args[i]);
	});
}

function pickHttpRule(rules: MockSpec["http"]["rules"], method: string, url: string) {
	if (!rules) return undefined;
	return rules.find((r) => r.method.toUpperCase() === method.toUpperCase() && r.url === url);
}

function installFetchMock(spec: MockSpec): () => void {
	const origFetch = globalThis.fetch;
	globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
		const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
		const method = (init?.method ?? "GET").toUpperCase();
		const match = pickHttpRule(spec.http?.rules, method, url);
		const resp = match?.response ?? spec.http?.default_response ?? { status: 404, body: "mock: no match" };
		return new Response(resp.body ?? "", { status: resp.status, headers: resp.headers ?? {} });
	};
	return () => { globalThis.fetch = origFetch; };
}

function wireRuntime(ext: any, spec: MockSpec): any {
	const runtime = ext.__runtime ?? {};

	let sessionName = spec.session?.name ?? (spec.session?.state as any)?.sessionName;
	runtime.sendMessage = () => {};
	runtime.sendUserMessage = () => {};
	runtime.appendEntry = () => {};
	runtime.setSessionName = (name: string) => { sessionName = name; };
	runtime.getSessionName = () => sessionName;
	runtime.setLabel = () => {};
	runtime.getActiveTools = () => spec.tools?.active_tools ?? [];
	runtime.getAllTools = () => spec.tools?.all_tools ?? [];
	runtime.setActiveTools = () => {};
	runtime.setModel = async () => true;
	runtime.getThinkingLevel = () => spec.model?.thinking_level ?? "off";
	runtime.setThinkingLevel = () => {};
	runtime.exec = async (command: string, args: string[], cwd: string) => {
		const match = pickExecRule(spec.exec?.rules, command, args);
		return match?.result ?? spec.exec?.default_result ?? { stdout: "", stderr: "mock: not found", code: 127, killed: false };
	};
	runtime.flagValues = new Map();
	runtime.pendingProviderRegistrations = [];

	return runtime;
}

async function main() {
	applyDeterministicGlobals();
	const args = process.argv.slice(2);
	if (args.length < 2) {
		originalConsole.log(JSON.stringify({ success: false, error: "Usage: <extension-path> <mock-spec-path>" }));
		process.exit(1);
	}

	const extensionPath = path.resolve(args[0]);
	const mockSpecPath = path.resolve(args[1]);
	const spec: MockSpec = readJson(mockSpecPath) as any ?? {};

	const stdinData = await readStdin();
	let scenario: ScenarioInput;
	try {
		scenario = JSON.parse(stdinData) as ScenarioInput;
	} catch {
		originalConsole.log(JSON.stringify({ success: false, error: `Invalid scenario JSON on stdin: ${stdinData.slice(0, 200)}` }));
		process.exit(1);
		return;
	}

	const restoreFetch = installFetchMock(spec);

	try {
		const loadStart = performance.now();
		const result = await loadExtensions([extensionPath], process.cwd());
		const loadTimeMs = Math.round(performance.now() - loadStart);

		if (result.errors.length > 0) {
			originalConsole.log(JSON.stringify({
				success: false,
				error: result.errors.map((e: any) => `${e.path}: ${e.error}`).join("; "),
				load_time_ms: loadTimeMs,
			}));
			return;
		}

		if (result.extensions.length === 0) {
			originalConsole.log(JSON.stringify({ success: false, error: "No extension loaded", load_time_ms: loadTimeMs }));
			return;
		}

		const ext = result.extensions[0];
		const runtime = result.runtime;
		// Wire runtime mocks
		wireRuntime({ __runtime: runtime }, spec);

		const execStart = performance.now();
		let execResult: JsonValue = null;
		let execError: string | null = null;

		switch (scenario.kind) {
			case "tool": {
				const toolName = scenario.tool_name;
				if (!toolName) { execError = "tool scenario missing tool_name"; break; }
				const toolEntry = ext.tools.get(toolName);
				if (!toolEntry) { execError = `tool '${toolName}' not registered`; break; }
				const def = toolEntry.definition as any;
				if (typeof def.execute !== "function") { execError = `tool '${toolName}' has no execute handler`; break; }
				const input = (scenario.input as any)?.arguments ?? {};
				const toolCallId = `tc-ts-${toolName}`;
				try {
					// Build a minimal tool context
					const hasUI = (scenario.input as any)?.ctx?.has_ui ?? false;
					const ctx = {
						hasUI,
						cwd: process.cwd(),
						ui: { notify: () => {}, setWidget: () => {}, custom: async () => {} },
						sessionManager: {
							getState: () => spec.session?.state ?? {},
							getEntries: () => spec.session?.entries ?? [],
							getBranch: () => spec.session?.branch ?? [],
						},
					};
					// execute(toolCallId, params, signal, onUpdate, ctx)
					const signal = new AbortController().signal;
					const onUpdate = () => {};
					execResult = await def.execute(toolCallId, input, signal, onUpdate, ctx) as JsonValue;
				} catch (err: any) {
					execError = err?.message ?? String(err);
				}
				break;
			}
			case "command": {
				const cmdName = scenario.command_name;
				if (!cmdName) { execError = "command scenario missing command_name"; break; }
				const cmdEntry = ext.commands.get(cmdName);
				if (!cmdEntry) { execError = `command '${cmdName}' not registered`; break; }
				if (typeof cmdEntry.handler !== "function") { execError = `command '${cmdName}' has no handler`; break; }
				const cmdArgs = (scenario.input as any)?.args ?? "";
				try {
					const hasUI = (scenario.input as any)?.ctx?.has_ui ?? false;
					const ctx = {
						hasUI,
						cwd: process.cwd(),
						ui: { notify: () => {}, setWidget: () => {}, custom: async () => {} },
						sessionManager: {
							getState: () => spec.session?.state ?? {},
							getEntries: () => spec.session?.entries ?? [],
							getBranch: () => spec.session?.branch ?? [],
						},
					};
					execResult = await cmdEntry.handler(cmdArgs, ctx) as JsonValue;
				} catch (err: any) {
					execError = err?.message ?? String(err);
				}
				break;
			}
			case "event": {
				const eventName = scenario.event_name;
				if (!eventName) { execError = "event scenario missing event_name"; break; }
				const handlers = ext.handlers.get(eventName);
				if (!handlers || handlers.length === 0) { execError = `no handlers for event '${eventName}'`; break; }
				const eventPayload = (scenario.input as any)?.event ?? {};
				try {
					const hasUI = (scenario.input as any)?.ctx?.has_ui ?? false;
					const ctx = {
						hasUI,
						cwd: process.cwd(),
						ui: { notify: () => {}, setWidget: () => {}, custom: async () => {} },
						sessionManager: {
							getState: () => spec.session?.state ?? {},
							getEntries: () => spec.session?.entries ?? [],
							getBranch: () => spec.session?.branch ?? [],
						},
					};
					// Dispatch to all handlers, collect results
					const results: JsonValue[] = [];
					for (const handler of handlers) {
						const r = await handler(eventPayload, ctx);
						results.push(r as JsonValue);
					}
					// If single handler, return its result directly; otherwise array
					execResult = results.length === 1 ? results[0] : results;
				} catch (err: any) {
					execError = err?.message ?? String(err);
				}
				break;
			}
			case "provider": {
				// Provider scenarios check registration only
				const providers = runtime.pendingProviderRegistrations ?? [];
				execResult = {
					providers: providers.map((p: any) => ({
						name: p.name,
						models: (p.config?.models ?? []).map((m: any) => ({ id: m.id ?? null, name: m.name ?? null })),
						api: p.config?.api ?? null,
						apiKey: p.config?.apiKey ?? p.config?.apiKeyEnvVar ?? null,
					})),
				} as JsonValue;
				break;
			}
			default:
				execError = `unsupported scenario kind: ${scenario.kind}`;
		}

		const execTimeMs = Math.round(performance.now() - execStart);

		originalConsole.log(JSON.stringify({
			success: execError === null,
			kind: scenario.kind,
			error: execError,
			result: execResult,
			load_time_ms: loadTimeMs,
			exec_time_ms: execTimeMs,
		}));
	} catch (err: any) {
		originalConsole.log(JSON.stringify({
			success: false,
			error: err?.message ? `${err.message}\n${err.stack}` : String(err),
			kind: scenario.kind,
			result: null,
			load_time_ms: null,
			exec_time_ms: null,
		}));
	} finally {
		restoreFetch();
		process.exit(0);
	}
}

main();
