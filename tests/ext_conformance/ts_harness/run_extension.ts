/**
 * TS Conformance Harness: load a pi-mono extension with a mock runtime and
 * emit a deterministic JSON snapshot + captured hostcall invocations.
 *
 * Usage (from repo root):
 *   bun run tests/ext_conformance/ts_harness/run_extension.ts <extension-path> <mock-spec-path> [cwd]
 *
 * Optional env:
 * - PI_TS_CAPTURE_LOGS=1  Capture console output from extensions into JSON output (suppresses stdout noise).
 *
 * Notes:
 * - This harness uses pi-mono's loader from the compiled dist/ output.
 * - It injects runtime action mocks (sendMessage, setModel, etc).
 * - It replaces global fetch with a mock based on the mock spec.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

interface MockSpec {
	schema?: string;
	extension_id?: string;
	description?: string;
	session?: {
		name?: string;
		file?: string;
		state?: JsonValue;
		messages?: JsonValue[];
		entries?: JsonValue[];
		branch?: JsonValue[];
		accept_mutations?: boolean;
	};
	http?: {
		rules?: HttpRule[];
		default_response?: HttpResponse;
	};
	exec?: {
		rules?: ExecRule[];
		default_result?: ExecResult;
	};
	tools?: {
		active_tools?: string[];
		all_tools?: Array<{ name: string; description?: string }>;
		invocations?: JsonValue[];
	};
	ui?: {
		capture?: boolean;
		responses?: Record<string, JsonValue>;
		confirm_default?: boolean;
		dialog_default?: string;
	};
	events?: {
		fire_sequence?: JsonValue[];
	};
	model?: {
		current?: { provider?: string; model_id?: string; name?: string };
		thinking_level?: string;
		available_models?: JsonValue[];
		accept_mutations?: boolean;
	};
}

interface ExecRule {
	command: string;
	args?: string[];
	result: ExecResult;
}

interface ExecResult {
	stdout: string;
	stderr: string;
	code: number;
	killed?: boolean;
}

interface HttpRule {
	method: string;
	url: string;
	response: HttpResponse;
}

interface HttpResponse {
	status: number;
	headers?: Record<string, string>;
	body?: string;
}

interface CaptureLog {
	sendMessage: Array<{ message: JsonValue; options?: JsonValue }>;
	sendUserMessage: Array<{ content: JsonValue; options?: JsonValue }>;
	appendEntry: Array<{ customType: string; data?: JsonValue }>;
	setSessionName: Array<{ name: string }>;
	setLabel: Array<{ entryId: string; label?: string }>;
	setActiveTools: Array<{ tools: string[] }>;
	setModel: Array<{ model: JsonValue }>;
	setThinkingLevel: Array<{ level: string }>;
	exec: Array<{ command: string; args: string[]; cwd: string; matched: boolean } & ExecResult>;
	http: Array<{ method: string; url: string; matched: boolean; response: HttpResponse }>;
	ui: Array<{ op: string; payload?: JsonValue; result?: JsonValue }>;
	warnings: string[];
}

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const PI_MONO_ROOT = path.resolve(__dirname, "../../../legacy_pi_mono_code/pi-mono");

const loaderPath = path.join(PI_MONO_ROOT, "packages/coding-agent/dist/core/extensions/loader.js");
const { loadExtensions } = await import(loaderPath);

const CAPTURE_LOGS = process.env.PI_TS_CAPTURE_LOGS === "1";
const FORCE_EXIT = process.env.PI_TS_FORCE_EXIT !== "0";
const capturedLogs: Array<{ level: "log" | "warn" | "error"; message: string }> = [];
const originalConsole = {
	log: console.log.bind(console),
	warn: console.warn.bind(console),
	error: console.error.bind(console),
};

function serializeArgs(args: unknown[]): string {
	return args
		.map((arg) => {
			if (typeof arg === "string") return arg;
			try {
				return JSON.stringify(arg);
			} catch {
				return String(arg);
			}
		})
		.join(" ");
}

if (CAPTURE_LOGS) {
	console.log = (...args: unknown[]) => {
		capturedLogs.push({ level: "log", message: serializeArgs(args) });
	};
	console.warn = (...args: unknown[]) => {
		capturedLogs.push({ level: "warn", message: serializeArgs(args) });
	};
	console.error = (...args: unknown[]) => {
		capturedLogs.push({ level: "error", message: serializeArgs(args) });
	};
}

function readJson(filePath: string): JsonValue {
	const raw = fs.readFileSync(filePath, "utf-8");
	return JSON.parse(raw) as JsonValue;
}

function normalizeMockSpec(raw: JsonValue): MockSpec {
	if (!raw || typeof raw !== "object") return {};
	return raw as MockSpec;
}

function pickExecRule(rules: ExecRule[] | undefined, command: string, args: string[]): ExecRule | undefined {
	if (!rules) return undefined;
	return rules.find((rule) => {
		if (rule.command !== command) return false;
		if (!rule.args) return true;
		if (rule.args.length !== args.length) return false;
		return rule.args.every((val, idx) => val === args[idx]);
	});
}

function pickHttpRule(rules: HttpRule[] | undefined, method: string, url: string): HttpRule | undefined {
	if (!rules) return undefined;
	return rules.find((rule) => rule.method.toUpperCase() === method.toUpperCase() && rule.url === url);
}

function installFetchMock(spec: MockSpec, capture: CaptureLog): () => void {
	const originalFetch = globalThis.fetch;
	globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
		const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
		const method = (init?.method ?? (typeof input === "object" && "method" in input ? input.method : "GET")).toUpperCase();
		const match = pickHttpRule(spec.http?.rules, method, url);
		const response = match?.response ?? spec.http?.default_response ?? { status: 404, body: "mock: no HTTP rule matched" };
		capture.http.push({ method, url, matched: Boolean(match), response });
		return new Response(response.body ?? "", {
			status: response.status,
			headers: response.headers ?? {},
		});
	};
	return () => {
		globalThis.fetch = originalFetch;
	};
}

async function main() {
	const args = process.argv.slice(2);
	if (args.length < 2) {
		console.error("Usage: bun run tests/ext_conformance/ts_harness/run_extension.ts <extension-path> <mock-spec-path> [cwd]");
		process.exit(1);
	}

	const extensionPath = path.resolve(args[0]);
	const mockSpecPath = path.resolve(args[1]);
	const cwd = args[2] ? path.resolve(args[2]) : process.cwd();

	const spec = normalizeMockSpec(readJson(mockSpecPath));
	const capture: CaptureLog = {
		sendMessage: [],
		sendUserMessage: [],
		appendEntry: [],
		setSessionName: [],
		setLabel: [],
		setActiveTools: [],
		setModel: [],
		setThinkingLevel: [],
		exec: [],
		http: [],
		ui: [],
		warnings: [],
	};

	const restoreFetch = installFetchMock(spec, capture);

	let exitCode = 0;
	try {
		const loadStart = Date.now();
		const result = await loadExtensions([extensionPath], cwd);
		const loadTimeMs = Date.now() - loadStart;

		if (result.errors.length > 0) {
			originalConsole.log(
				JSON.stringify(
					{
						success: false,
						error: result.errors.map((e: { path: string; error: string }) => `${e.path}: ${e.error}`).join("; "),
						extension: null,
						load_time_ms: loadTimeMs,
						logs: CAPTURE_LOGS ? capturedLogs : undefined,
					},
					null,
					2,
				),
			);
			return;
		}

		if (result.extensions.length === 0) {
			originalConsole.log(
				JSON.stringify(
					{
						success: false,
						error: "No extension loaded (empty result)",
						extension: null,
						load_time_ms: loadTimeMs,
						logs: CAPTURE_LOGS ? capturedLogs : undefined,
					},
					null,
					2,
				),
			);
			return;
		}

		const ext = result.extensions[0];
		const runtime = result.runtime as {
			sendMessage: (message: JsonValue, options?: JsonValue) => void;
			sendUserMessage: (content: JsonValue, options?: JsonValue) => void;
			appendEntry: (customType: string, data?: JsonValue) => void;
			setSessionName: (name: string) => void;
			getSessionName: () => string | undefined;
			setLabel: (entryId: string, label?: string) => void;
			getActiveTools: () => string[];
			getAllTools: () => Array<{ name: string; description?: string }>;
			setActiveTools: (toolNames: string[]) => void;
			setModel: (model: JsonValue) => Promise<boolean>;
			getThinkingLevel: () => string;
			setThinkingLevel: (level: string) => void;
			exec?: (command: string, args: string[], cwd: string, options?: JsonValue) => Promise<ExecResult>;
			flagValues: Map<string, boolean | string>;
			pendingProviderRegistrations: Array<{ name: string; config: { models?: Array<{ id?: string; name?: string }> } }>;
		};

		let sessionName = spec.session?.name ?? (spec.session?.state as any)?.sessionName;
		const acceptSessionMutations = spec.session?.accept_mutations ?? true;
		const acceptModelMutations = spec.model?.accept_mutations ?? true;

		runtime.sendMessage = (message, options) => {
			capture.sendMessage.push({ message, options });
		};
		runtime.sendUserMessage = (content, options) => {
			capture.sendUserMessage.push({ content, options });
		};
		runtime.appendEntry = (customType, data) => {
			capture.appendEntry.push({ customType, data });
			if (acceptSessionMutations && spec.session?.entries) {
				spec.session.entries.push({ customType, data });
			}
		};
		runtime.setSessionName = (name) => {
			capture.setSessionName.push({ name });
			if (acceptSessionMutations) {
				sessionName = name;
				if (spec.session?.state && typeof spec.session.state === "object" && spec.session.state) {
					(spec.session.state as Record<string, JsonValue>)["sessionName"] = name;
				}
			}
		};
		runtime.getSessionName = () => sessionName;
		runtime.setLabel = (entryId, label) => {
			capture.setLabel.push({ entryId, label });
		};
		runtime.getActiveTools = () => spec.tools?.active_tools ?? [];
		runtime.getAllTools = () => spec.tools?.all_tools ?? [];
		runtime.setActiveTools = (toolNames) => {
			capture.setActiveTools.push({ tools: toolNames });
			if (acceptSessionMutations && spec.tools) {
				spec.tools.active_tools = [...toolNames];
			}
		};
		runtime.setModel = async (model) => {
			capture.setModel.push({ model });
			if (acceptModelMutations && spec.model) {
				spec.model.current = {
					...(spec.model.current ?? {}),
					provider: (model as any)?.provider ?? spec.model.current?.provider,
					model_id: (model as any)?.id ?? (model as any)?.model_id ?? spec.model.current?.model_id,
					name: (model as any)?.name ?? spec.model.current?.name,
				};
			}
			return true;
		};
		runtime.getThinkingLevel = () => spec.model?.thinking_level ?? "off";
		runtime.setThinkingLevel = (level) => {
			capture.setThinkingLevel.push({ level });
			if (acceptModelMutations && spec.model) {
				spec.model.thinking_level = level;
			}
		};
		runtime.exec = async (command, args, execCwd) => {
			const match = pickExecRule(spec.exec?.rules, command, args);
			const resultValue = match?.result ?? spec.exec?.default_result ?? {
				stdout: "",
				stderr: "mock: command not found",
				code: 127,
				killed: false,
			};
			capture.exec.push({
				command,
				args,
				cwd: execCwd,
				matched: Boolean(match),
				stdout: resultValue.stdout,
				stderr: resultValue.stderr,
				code: resultValue.code,
				killed: resultValue.killed ?? false,
			});
			return resultValue;
		};

		if (spec.events?.fire_sequence?.length) {
			capture.warnings.push("events.fire_sequence provided but event firing is not implemented in this harness");
		}

		const handlers: Record<string, number> = {};
		for (const [event, fns] of ext.handlers) {
			handlers[event] = fns.length;
		}

		const tools = [];
		for (const [, registered] of ext.tools) {
			const def = registered.definition as any;
			tools.push({
				name: def.name,
				label: def.label ?? null,
				description: def.description ?? null,
				parameters: def.parameters ?? null,
				hasExecute: typeof def.execute === "function",
			});
		}

		const commands = [];
		for (const [, cmd] of ext.commands) {
			commands.push({
				name: cmd.name,
				description: cmd.description ?? null,
				userFacing: (cmd as any).userFacing ?? false,
				hasHandler: typeof cmd.handler === "function",
			});
		}

		const shortcuts = [];
		for (const [, sc] of ext.shortcuts) {
			shortcuts.push({
				shortcut: sc.shortcut,
				description: sc.description ?? null,
				hasHandler: typeof sc.handler === "function",
			});
		}

		const flags = [];
		for (const [, flag] of ext.flags) {
			flags.push({
				name: flag.name,
				type: flag.type,
				default: (flag as any).default ?? null,
				description: flag.description ?? null,
			});
		}

		const messageRenderers = Array.from(ext.messageRenderers.keys());
		const providers = runtime.pendingProviderRegistrations.map((p) => ({
			name: p.name,
			models: (p.config.models ?? []).map((m) => ({ id: m.id ?? null, name: m.name ?? null })),
		}));
		const flagValues: Record<string, boolean | string> = {};
		for (const [k, v] of runtime.flagValues) {
			flagValues[k] = v;
		}

		const output = {
			success: true,
			error: null,
			load_time_ms: loadTimeMs,
			spec: {
				path: mockSpecPath,
				schema: spec.schema ?? null,
				extension_id: spec.extension_id ?? null,
			},
			extension: {
				path: ext.path,
				resolvedPath: ext.resolvedPath,
				handlers,
				tools,
				commands,
				shortcuts,
				flags,
				messageRenderers,
				providers,
				flagValues,
			},
			runtime: {
				sessionName,
				activeTools: spec.tools?.active_tools ?? [],
				allTools: spec.tools?.all_tools ?? [],
				model: spec.model?.current ?? null,
				thinkingLevel: spec.model?.thinking_level ?? "off",
			},
			capture,
			logs: CAPTURE_LOGS ? capturedLogs : undefined,
		};

		originalConsole.log(JSON.stringify(output, null, 2));
	} catch (err) {
		const output = {
			success: false,
			error: err instanceof Error ? `${err.message}\n${err.stack}` : String(err),
			extension: null,
			load_time_ms: null,
			logs: CAPTURE_LOGS ? capturedLogs : undefined,
		};
		originalConsole.log(JSON.stringify(output, null, 2));
	} finally {
		restoreFetch();
		if (FORCE_EXIT) {
			process.exit(exitCode);
		}
	}
}

main();
