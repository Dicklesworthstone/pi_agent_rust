import { afterEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { loadExtensionConfig } from "./config";
import { DEFAULT_EXTENSION_CONFIG } from "./types";

let tempDirs: string[] = [];

function writeTempConfig(content: string): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-better-compaction-config-"));
	tempDirs.push(dir);
	const configPath = path.join(dir, "config.json");
	fs.writeFileSync(configPath, content, "utf8");
	return configPath;
}

afterEach(() => {
	for (const dir of tempDirs) {
		fs.rmSync(dir, { recursive: true, force: true });
	}
	tempDirs = [];
});

describe("loadExtensionConfig", () => {
	test("missing file yields defaults without warnings", () => {
		const missingPath = path.join(os.tmpdir(), "pi-better-compaction-missing", "config.json");
		const loaded = loadExtensionConfig(missingPath);

		expect(loaded.source).toBeUndefined();
		expect(loaded.warnings).toEqual([]);
		expect(loaded.config.enabled).toBe(true);
		expect(loaded.config.allowCompactionContinuityBreak).toBe(false);
		expect(loaded.config.compactionModel).toBeUndefined();
		expect(loaded.config.compactionThinkingLevel).toBe("off");
		expect(loaded.config.responsesCompactApis).toEqual([...DEFAULT_EXTENSION_CONFIG.responsesCompactApis]);
	});

	test("config file overrides defaults", () => {
		const configPath = writeTempConfig(
			JSON.stringify({
				enabled: true,
				allowCompactionContinuityBreak: true,
				compactionModel: " google/gemini-2.5-flash ",
				compactionThinkingLevel: "medium",
				responsesCompactApis: ["openai-responses"],
				debug: true,
				notifyOnLoad: true,
				artifactRoot: "~/artifacts/pbc",
			}),
		);

		const loaded = loadExtensionConfig(configPath);

		expect(loaded.source).toBe(configPath);
		expect(loaded.warnings).toEqual([]);
		expect(loaded.config.allowCompactionContinuityBreak).toBe(true);
		expect(loaded.config.compactionModel).toBe("google/gemini-2.5-flash");
		expect(loaded.config.compactionThinkingLevel).toBe("medium");
		expect(loaded.config.responsesCompactApis).toEqual(["openai-responses"]);
		expect(loaded.config.debug).toBe(true);
		expect(loaded.config.notifyOnLoad).toBe(true);
		expect(loaded.config.artifactRoot).toBe(path.join(os.homedir(), "artifacts/pbc"));
	});

	test("compactionModel null clears the spec", () => {
		const configPath = writeTempConfig(JSON.stringify({ compactionModel: null }));
		const loaded = loadExtensionConfig(configPath);

		expect(loaded.config.compactionModel).toBeUndefined();
		expect(loaded.warnings).toEqual([]);
	});

	test("invalid fields warn and fall back to defaults", () => {
		const configPath = writeTempConfig(
			JSON.stringify({
				enabled: "yes",
				allowCompactionContinuityBreak: "yes",
				compactionModel: 42,
				compactionThinkingLevel: "ultra",
				responsesCompactApis: ["openai-responses", "anthropic-messages"],
				artifactRoot: "",
			}),
		);

		const loaded = loadExtensionConfig(configPath);

		expect(loaded.config.enabled).toBe(true);
		expect(loaded.config.allowCompactionContinuityBreak).toBe(false);
		expect(loaded.config.compactionModel).toBeUndefined();
		expect(loaded.config.compactionThinkingLevel).toBe("off");
		// The valid entry is kept; the incapable API is dropped with a warning.
		expect(loaded.config.responsesCompactApis).toEqual(["openai-responses"]);
		expect(loaded.warnings.length).toBe(6);
	});

	test("malformed JSON warns and yields defaults", () => {
		const configPath = writeTempConfig("{ not json");
		const loaded = loadExtensionConfig(configPath);

		expect(loaded.source).toBeUndefined();
		expect(loaded.warnings.length).toBe(1);
		expect(loaded.config.enabled).toBe(true);
	});

	test("relative artifactRoot resolves against the config directory", () => {
		const configPath = writeTempConfig(JSON.stringify({ artifactRoot: "artifacts" }));
		const loaded = loadExtensionConfig(configPath);

		expect(loaded.config.artifactRoot).toBe(path.resolve(path.dirname(configPath), "artifacts"));
	});
});
