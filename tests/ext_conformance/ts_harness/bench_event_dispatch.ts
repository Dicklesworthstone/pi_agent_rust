/**
 * TS Benchmark Harness: fire extension events and measure dispatch latency.
 *
 * Usage (from repo root):
 *   bun run tests/ext_conformance/ts_harness/bench_event_dispatch.ts <extension-path> [event-payloads-json] [cwd]
 *
 * Output: JSON report (stdout).
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

// Resolve pi-mono root relative to this script's location.
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const PI_MONO_ROOT = path.resolve(__dirname, "../../../legacy_pi_mono_code/pi-mono");

const loaderPath = path.join(PI_MONO_ROOT, "packages/coding-agent/dist/core/extensions/loader.js");
const { loadExtensions } = await import(loaderPath);

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

function applyDeterministicGlobals() {
	const timeRaw = process.env.PI_DETERMINISTIC_TIME_MS;
	const stepRaw = process.env.PI_DETERMINISTIC_TIME_STEP_MS;
	if (timeRaw && timeRaw.trim().length > 0) {
		const base = Number(timeRaw);
		if (Number.isFinite(base)) {
			const stepValue = stepRaw ? Number(stepRaw) : 1;
			const step = Number.isFinite(stepValue) ? stepValue : 1;
			let tick = 0;
			const nextNow = () => {
				const value = base + step * tick;
				tick += 1;
				return value;
			};
			const OriginalDate = Date;
			class DeterministicDate extends OriginalDate {
				constructor(...args: any[]) {
					if (args.length === 0) {
						super(nextNow());
					} else {
						super(...args);
					}
				}
				static now() {
					return nextNow();
				}
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
		if (Number.isFinite(value)) {
			Math.random = () => value;
		}
	} else if (randSeedRaw && randSeedRaw.trim().length > 0) {
		let state = Number(randSeedRaw);
		if (Number.isFinite(state)) {
			state = state >>> 0;
			Math.random = () => {
				state = (state * 1664525 + 1013904223) >>> 0;
				return state / 4294967296;
			};
		}
	}

	const detCwd = process.env.PI_DETERMINISTIC_CWD;
	if (detCwd && detCwd.trim().length > 0) {
		try {
			Object.defineProperty(process, "cwd", {
				value: () => detCwd,
				configurable: true,
			});
		} catch {}
	}

	const detHome = process.env.PI_DETERMINISTIC_HOME;
	if (detHome && detHome.trim().length > 0) {
		try {
			process.env.HOME = detHome;
			process.env.USERPROFILE = detHome;
		} catch {}
	}
}

function readJson(filePath: string): JsonValue {
	const raw = fs.readFileSync(filePath, "utf-8");
	return JSON.parse(raw) as JsonValue;
}

function nowNs(): bigint {
	return process.hrtime.bigint();
}

function nsToUs(ns: bigint): number {
	return Number(ns) / 1000;
}

function percentileIndex(len: number, numerator: number, denominator: number): number {
	if (len === 0) return 0;
	const rank = Math.floor((len * numerator + (denominator - 1)) / denominator);
	return Math.min(Math.max(rank - 1, 0), len - 1);
}

function summarize(valuesUs: number[]) {
	if (valuesUs.length === 0) return { count: 0 };
	const sorted = [...valuesUs].sort((a, b) => a - b);
	return {
		count: sorted.length,
		min_us: sorted[0],
		max_us: sorted[sorted.length - 1],
		p50_us: sorted[percentileIndex(sorted.length, 1, 2)],
		p95_us: sorted[percentileIndex(sorted.length, 95, 100)],
		p99_us: sorted[percentileIndex(sorted.length, 99, 100)],
	};
}

function adaptInputPayload(payload: any) {
	const text = typeof payload?.text === "string" ? payload.text : typeof payload?.content === "string" ? payload.content : "";
	const images = Array.isArray(payload?.images) ? payload.images : Array.isArray(payload?.attachments) ? payload.attachments : [];
	const source = payload?.source ?? "user";
	return { type: "input", text, images, source };
}

async function dispatchEvent(ext: any, eventName: string, event: any, ctx: any) {
	const handlers: any[] = ext.handlers?.get?.(eventName) ?? [];
	let last: any = undefined;

	for (const handler of handlers) {
		if (typeof handler !== "function") continue;
		const value = await handler(event, ctx);
		if (value === undefined) continue;

		// First-result semantics (legacy parity).
		if (eventName === "user_bash") return value;

		last = value;

		// Early-stop semantics (legacy parity).
		if (eventName === "tool_call" && value && typeof value === "object" && value.block) return value;
		if (eventName.startsWith("session_before_") && value && typeof value === "object" && value.cancel) return value;
	}

	return last;
}

async function benchOne(ext: any, ctx: any, eventName: string, payloadCases: any[], iters: number, warmup: number) {
	if (!payloadCases.length) return { summary: { count: 0 } };
	const durationsUs: number[] = [];

	// Warmup
	for (let i = 0; i < warmup; i++) {
		const base = payloadCases[i % payloadCases.length];
		const payload = base?.payload ?? base;
		const event = eventName === "input" ? adaptInputPayload(payload) : payload;
		await dispatchEvent(ext, eventName, event, ctx);
	}

	for (let i = 0; i < iters; i++) {
		const base = payloadCases[i % payloadCases.length];
		const payload = base?.payload ?? base;
		const event = eventName === "input" ? adaptInputPayload(payload) : payload;
		const start = nowNs();
		await dispatchEvent(ext, eventName, event, ctx);
		const end = nowNs();
		durationsUs.push(nsToUs(end - start));
	}

	return { summary: summarize(durationsUs) };
}

async function main() {
	applyDeterministicGlobals();
	const args = process.argv.slice(2);
	if (args.length < 1) {
		console.error(
			"Usage: bun run tests/ext_conformance/ts_harness/bench_event_dispatch.ts <extension-path> [event-payloads-json] [cwd]",
		);
		process.exit(1);
	}

	const extensionPath = path.resolve(args[0]);
	const payloadsPath = args[1]
		? path.resolve(args[1])
		: path.resolve(__dirname, "../event_payloads/event_payloads.json");
	const envCwd = process.env.PI_DETERMINISTIC_CWD;
	const cwd = args[2] ? path.resolve(args[2]) : envCwd ? path.resolve(envCwd) : process.cwd();

	const iters = Number(process.env.PI_EVENT_BENCH_ITERS ?? "1000");
	const warmup = Number(process.env.PI_EVENT_BENCH_WARMUP ?? "25");
	const timeoutMs = Number(process.env.PI_TS_ORACLE_TIMEOUT_MS ?? "20000");

	const ctx = {
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
			theme: {},
		},
		hasUI: false,
		cwd,
		sessionManager: {
			getEntries: () => [],
			getBranch: () => [],
			getLeafEntry: () => null,
		},
		modelRegistry: {
			getApiKeyForProvider: async (_provider: string) => undefined,
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

	try {
		let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
		const timeout = new Promise<never>((_, reject) => {
			timeoutHandle = setTimeout(() => {
				reject(new Error(`loadExtensions timeout after ${timeoutMs}ms`));
			}, timeoutMs);
		});

		const result = await Promise.race([loadExtensions([extensionPath], cwd), timeout]);
		if (timeoutHandle) clearTimeout(timeoutHandle);

		if (result.errors.length > 0 || result.extensions.length === 0) {
			const output = {
				success: false,
				error: result.errors.map((e: any) => `${e.path}: ${e.error}`).join("; ") || "No extension loaded",
				report: null,
			};
			console.log(JSON.stringify(output, null, 2));
			process.exit(0);
		}

		const ext = result.extensions[0];
		const payloadRoot = readJson(payloadsPath) as any;
		const payloads = payloadRoot?.event_payloads ?? {};

		const eventNames = [
			"tool_call",
			"tool_result",
			"turn_start",
			"turn_end",
			"before_agent_start",
			"input",
			"context",
			"resources_discover",
			"user_bash",
			"session_before_compact",
			"session_before_tree",
		];

		const results: Record<string, any> = {};
		for (const name of eventNames) {
			results[name] = await benchOne(ext, ctx, name, payloads[name] ?? [], iters, warmup);
		}

		const report = {
			schema: "pi.ext.event_dispatch_latency.v1",
			generated_at: new Date().toISOString(),
			toolchain: "ts",
			iters,
			warmup,
			extension: {
				path: ext.path,
				resolvedPath: ext.resolvedPath,
			},
			results,
		};

		console.log(JSON.stringify({ success: true, error: null, report }, null, 2));
		process.exit(0);
	} catch (err) {
		const output = {
			success: false,
			error: err instanceof Error ? `${err.message}\n${err.stack}` : String(err),
			report: null,
		};
		console.log(JSON.stringify(output, null, 2));
		process.exit(0);
	}
}

main();

