import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ThinkingLevel } from "@earendil-works/pi-agent-core";
import {
	DEFAULT_EXTENSION_CONFIG,
	EXTENSION_ID,
	RESPONSES_COMPACT_CAPABLE_APIS,
	THINKING_LEVELS,
	type ExtensionConfig,
	type LoadedExtensionConfig,
} from "./types";

export const CONFIG_DIR = path.join(os.homedir(), ".pi", "agent", "extensions", EXTENSION_ID);
export const CONFIG_PATH = path.join(CONFIG_DIR, "config.json");

function isRecord(value: unknown): value is Record<string, unknown> {
	return !!value && typeof value === "object" && !Array.isArray(value);
}

function isFile(filePath: string): boolean {
	try {
		return fs.statSync(filePath).isFile();
	} catch {
		return false;
	}
}

function readJsonObject(filePath: string, warnings: string[]): Record<string, unknown> | undefined {
	if (!isFile(filePath)) return undefined;

	try {
		const parsed = JSON.parse(fs.readFileSync(filePath, "utf8"));
		if (isRecord(parsed)) return parsed;
		warnings.push(`Ignoring ${filePath}: expected a JSON object at the top level.`);
		return undefined;
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		warnings.push(`Ignoring ${filePath}: ${message}`);
		return undefined;
	}
}

function resolveConfiguredPath(rawPath: string, baseDir: string): string {
	if (rawPath.startsWith("~/")) {
		return path.join(os.homedir(), rawPath.slice(2));
	}
	if (path.isAbsolute(rawPath)) {
		return path.resolve(rawPath);
	}
	return path.resolve(baseDir, rawPath);
}

function toBoolean(value: unknown, fieldPath: string, warnings: string[]): boolean | undefined {
	if (value === undefined) return undefined;
	if (typeof value === "boolean") return value;
	warnings.push(`Ignoring ${fieldPath}: expected a boolean.`);
	return undefined;
}

function toModelSpec(value: unknown, fieldPath: string, warnings: string[]): string | null | undefined {
	if (value === undefined) return undefined;
	// Explicit null clears a spec, matching the documented "unset = current model" behavior.
	if (value === null) return null;
	if (typeof value === "string" && value.trim().length > 0) {
		return value.trim();
	}
	warnings.push(`Ignoring ${fieldPath}: expected "provider/model-id" or null.`);
	return undefined;
}

function toThinkingLevel(value: unknown, fieldPath: string, warnings: string[]): ThinkingLevel | undefined {
	if (value === undefined) return undefined;
	if (typeof value === "string" && (THINKING_LEVELS as readonly string[]).includes(value)) {
		return value as ThinkingLevel;
	}
	warnings.push(`Ignoring ${fieldPath}: expected one of ${THINKING_LEVELS.join(", ")}.`);
	return undefined;
}

function toResponsesCompactApis(value: unknown, fieldPath: string, warnings: string[]): string[] | undefined {
	if (value === undefined) return undefined;
	if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
		warnings.push(`Ignoring ${fieldPath}: expected a string array.`);
		return undefined;
	}

	const capable = new Set<string>(RESPONSES_COMPACT_CAPABLE_APIS);
	const accepted: string[] = [];
	for (const item of new Set(value.map((entry) => entry.trim()).filter(Boolean))) {
		if (capable.has(item)) {
			accepted.push(item);
		} else {
			warnings.push(
				`Ignoring ${fieldPath} entry "${item}": only ${[...RESPONSES_COMPACT_CAPABLE_APIS].join(", ")} support the compact endpoint.`,
			);
		}
	}

	return accepted;
}

/**
 * Load extension config. Single source: `~/.pi/agent/extensions/pi-better-compaction/config.json`
 * merged over code defaults. A missing file silently yields the defaults.
 */
export function loadExtensionConfig(configPath: string = CONFIG_PATH): LoadedExtensionConfig {
	const warnings: string[] = [];
	const resolved: ExtensionConfig = {
		...DEFAULT_EXTENSION_CONFIG,
		responsesCompactApis: [...DEFAULT_EXTENSION_CONFIG.responsesCompactApis],
	};
	let source: string | undefined;

	const raw = readJsonObject(configPath, warnings);
	if (raw) {
		source = configPath;

		resolved.enabled = toBoolean(raw.enabled, "enabled", warnings) ?? resolved.enabled;
		resolved.allowCompactionContinuityBreak =
			toBoolean(raw.allowCompactionContinuityBreak, "allowCompactionContinuityBreak", warnings) ??
			resolved.allowCompactionContinuityBreak;
		resolved.notifyOnLoad = toBoolean(raw.notifyOnLoad, "notifyOnLoad", warnings) ?? resolved.notifyOnLoad;
		resolved.debug = toBoolean(raw.debug, "debug", warnings) ?? resolved.debug;
		resolved.logProviderPayloads =
			toBoolean(raw.logProviderPayloads, "logProviderPayloads", warnings) ?? resolved.logProviderPayloads;
		resolved.logCompactResponses =
			toBoolean(raw.logCompactResponses, "logCompactResponses", warnings) ?? resolved.logCompactResponses;
		resolved.redactSensitiveData =
			toBoolean(raw.redactSensitiveData, "redactSensitiveData", warnings) ?? resolved.redactSensitiveData;

		const modelSpec = toModelSpec(raw.compactionModel, "compactionModel", warnings);
		if (modelSpec !== undefined) {
			resolved.compactionModel = modelSpec === null ? undefined : modelSpec;
		}

		resolved.compactionThinkingLevel =
			toThinkingLevel(raw.compactionThinkingLevel, "compactionThinkingLevel", warnings) ??
			resolved.compactionThinkingLevel;

		const apis = toResponsesCompactApis(raw.responsesCompactApis, "responsesCompactApis", warnings);
		if (apis !== undefined) {
			resolved.responsesCompactApis = apis;
		}

		if (typeof raw.artifactRoot === "string" && raw.artifactRoot.trim().length > 0) {
			resolved.artifactRoot = raw.artifactRoot.trim();
		} else if (raw.artifactRoot !== undefined) {
			warnings.push("Ignoring artifactRoot: expected a non-empty string.");
		}
	}

	resolved.artifactRoot = resolveConfiguredPath(resolved.artifactRoot, path.dirname(configPath));

	return {
		config: resolved,
		source,
		warnings,
	};
}
