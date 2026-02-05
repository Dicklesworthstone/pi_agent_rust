/**
 * TS Oracle Harness: Load an extension via pi-mono's loader and output
 * a canonical JSON snapshot of everything it registered.
 *
 * Usage:
 *   bun run load_extension.ts <path-to-extension.ts> [cwd]
 *
 * MUST be run from the pi-mono root (for node_modules resolution).
 */

import * as path from "node:path";
import { fileURLToPath } from "node:url";

// Resolve pi-mono root relative to this script's location
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const PI_MONO_ROOT = path.resolve(__dirname, "../../../legacy_pi_mono_code/pi-mono");

// Import directly from the built loader to avoid pulling in the full package
// (which transitively requires AWS/Smithy/etc)
const loaderPath = path.join(PI_MONO_ROOT, "packages/coding-agent/dist/core/extensions/loader.js");
const { loadExtensions } = await import(loaderPath);

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

async function main() {
	applyDeterministicGlobals();
	const args = process.argv.slice(2);
	if (args.length < 1) {
		console.error("Usage: bun run load_extension.ts <extension-path> [cwd]");
		process.exit(1);
	}

	const extensionPath = path.resolve(args[0]);
	const envCwd = process.env.PI_DETERMINISTIC_CWD;
	const cwd = args[1] ? path.resolve(args[1]) : envCwd ? path.resolve(envCwd) : process.cwd();
	const timeoutMs = Number(process.env.PI_TS_ORACLE_TIMEOUT_MS ?? "20000");

	try {
		let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
		const timeout = new Promise<never>((_, reject) => {
			timeoutHandle = setTimeout(() => {
				reject(new Error(`loadExtensions timeout after ${timeoutMs}ms`));
			}, timeoutMs);
		});

		const result = await Promise.race([loadExtensions([extensionPath], cwd), timeout]);
		if (timeoutHandle) clearTimeout(timeoutHandle);

		if (result.errors.length > 0) {
			const output = {
				success: false,
				error: result.errors.map((e: any) => `${e.path}: ${e.error}`).join("; "),
				extension: null,
			};
			console.log(JSON.stringify(output, null, 2));
			process.exit(0);
		}

		if (result.extensions.length === 0) {
			const output = {
				success: false,
				error: "No extension loaded (empty result)",
				extension: null,
			};
			console.log(JSON.stringify(output, null, 2));
			process.exit(0);
		}

		const ext = result.extensions[0];

		// Serialize handlers: event name -> handler count
		const handlers: Record<string, number> = {};
		for (const [event, fns] of ext.handlers) {
			handlers[event] = fns.length;
		}

		// Serialize tools
		const tools = [];
		for (const [, registered] of ext.tools) {
			const def = registered.definition;
			tools.push({
				name: def.name,
				label: (def as any).label ?? null,
				description: def.description ?? null,
				parameters: def.parameters ?? null,
				hasExecute: typeof def.execute === "function",
			});
		}

		// Serialize commands
		const commands = [];
		for (const [, cmd] of ext.commands) {
			commands.push({
				name: cmd.name,
				description: cmd.description ?? null,
				userFacing: (cmd as any).userFacing ?? false,
				hasHandler: typeof cmd.handler === "function",
			});
		}

		// Serialize shortcuts
		const shortcuts = [];
		for (const [, sc] of ext.shortcuts) {
			shortcuts.push({
				shortcut: sc.shortcut,
				description: sc.description ?? null,
				hasHandler: typeof sc.handler === "function",
			});
		}

		// Serialize flags
		const flags = [];
		for (const [, flag] of ext.flags) {
			flags.push({
				name: flag.name,
				type: flag.type,
				default: (flag as any).default ?? null,
				description: flag.description ?? null,
			});
		}

		// Message renderers
		const messageRenderers = Array.from(ext.messageRenderers.keys());

		// Providers from runtime
		const providers = result.runtime.pendingProviderRegistrations.map((p: any) => ({
			name: p.name,
			models: (p.config.models ?? []).map((m: any) => ({
				id: m.id ?? null,
				name: m.name ?? null,
			})),
			hasStreamSimple: typeof p.config.streamSimple === "function",
			hasOauth: !!p.config.oauth,
		}));

		// Flag values
		const flagValues: Record<string, boolean | string> = {};
		for (const [k, v] of result.runtime.flagValues) {
			flagValues[k] = v;
		}

		const output = {
			success: true,
			error: null,
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
		};

		console.log(JSON.stringify(output, null, 2));
		// Extensions may install timers/handles (e.g. spinners) that keep the event
		// loop alive. The oracle is a one-shot snapshotter, so force exit after
		// printing to avoid hung differential tests.
		process.exit(0);
	} catch (err) {
		const output = {
			success: false,
			error: err instanceof Error ? `${err.message}\n${err.stack}` : String(err),
			extension: null,
		};
		console.log(JSON.stringify(output, null, 2));
		process.exit(0);
	}
}

main();
