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

async function main() {
	const args = process.argv.slice(2);
	if (args.length < 1) {
		console.error("Usage: bun run load_extension.ts <extension-path> [cwd]");
		process.exit(1);
	}

	const extensionPath = path.resolve(args[0]);
	const cwd = args[1] ? path.resolve(args[1]) : process.cwd();

	try {
		const result = await loadExtensions([extensionPath], cwd);

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
	} catch (err) {
		const output = {
			success: false,
			error: err instanceof Error ? `${err.message}\n${err.stack}` : String(err),
			extension: null,
		};
		console.log(JSON.stringify(output, null, 2));
	}
}

main();
