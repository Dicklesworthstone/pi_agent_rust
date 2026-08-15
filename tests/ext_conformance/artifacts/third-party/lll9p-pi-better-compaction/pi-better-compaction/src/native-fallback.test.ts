import { afterEach, describe, expect, mock, test } from "bun:test";
import { DEFAULT_EXTENSION_CONFIG, type ExtensionConfig } from "./types";

let importCounter = 0;

async function loadNativeFallbackModule() {
	mock.module("@earendil-works/pi-coding-agent", () => ({
		compact: async () => {
			throw new Error("unexpected call to pi's real compact()");
		},
		convertToLlm: (messages: unknown[]) => messages,
	}));
	return import(`./native-fallback.ts?nf=${importCounter++}`);
}

type FakeModel = {
	provider: string;
	id: string;
	api?: string;
};

function createCtx(args: {
	currentModel?: FakeModel;
	registryModels?: FakeModel[];
	auth?: unknown;
	authError?: Error;
}) {
	return {
		model: args.currentModel,
		modelRegistry: {
			find: (provider: string, modelId: string) =>
				(args.registryModels ?? []).find((model) => model.provider === provider && model.id === modelId),
			getApiKeyAndHeaders: async () => {
				if (args.authError) {
					throw args.authError;
				}
				return args.auth ?? { ok: true, apiKey: "sk-fallback", headers: { "x-h": "1" }, env: { E: "1" } };
			},
		},
	} as never;
}

function createEvent(signal?: AbortSignal) {
	return {
		preparation: {
			firstKeptEntryId: "entry-keep",
			messagesToSummarize: [],
			turnPrefixMessages: [],
			isSplitTurn: false,
			tokensBefore: 1234,
			fileOps: { readFiles: [], modifiedFiles: [] },
			settings: { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 },
		},
		customInstructions: "focus on auth work",
		signal: signal ?? new AbortController().signal,
	} as never;
}

function createConfig(overrides: Partial<ExtensionConfig> = {}): ExtensionConfig {
	return {
		...DEFAULT_EXTENSION_CONFIG,
		responsesCompactApis: [...DEFAULT_EXTENSION_CONFIG.responsesCompactApis],
		...overrides,
	};
}

afterEach(() => {
	mock.restore();
});

describe("parseModelSpec", () => {
	test("splits on the first slash so model ids may contain slashes", async () => {
		const { parseModelSpec } = await loadNativeFallbackModule();

		expect(parseModelSpec("openai/gpt-5-mini")).toEqual({ provider: "openai", modelId: "gpt-5-mini" });
		expect(parseModelSpec("openrouter/deepseek/deepseek-chat-v3")).toEqual({
			provider: "openrouter",
			modelId: "deepseek/deepseek-chat-v3",
		});
		expect(parseModelSpec("  google/gemini-2.5-flash  ")).toEqual({
			provider: "google",
			modelId: "gemini-2.5-flash",
		});
	});

	test("rejects specs without both provider and model id", async () => {
		const { parseModelSpec } = await loadNativeFallbackModule();

		expect(parseModelSpec("gpt-5-mini")).toBeUndefined();
		expect(parseModelSpec("/gpt-5-mini")).toBeUndefined();
		expect(parseModelSpec("openai/")).toBeUndefined();
		expect(parseModelSpec("")).toBeUndefined();
	});
});

describe("runNativeFallbackCompaction", () => {
	test("returns no-model-configured when compactionModel is unset", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({}),
			event: createEvent(),
			config: createConfig(),
		});

		expect(result).toEqual({ ok: false, reason: "no-model-configured" });
	});

	test("returns invalid-model-spec for malformed specs", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({}),
			event: createEvent(),
			config: createConfig({ compactionModel: "not-a-spec" }),
		});

		expect(result).toEqual({ ok: false, reason: "invalid-model-spec", modelSpec: "not-a-spec" });
	});

	test("returns model-not-found when the registry cannot resolve the spec", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({ registryModels: [] }),
			event: createEvent(),
			config: createConfig({ compactionModel: "google/gemini-2.5-flash" }),
		});

		expect(result).toEqual({ ok: false, reason: "model-not-found", modelSpec: "google/gemini-2.5-flash" });
	});

	test("returns same-as-current-model so pi's default path keeps streaming UI", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();
		const model = { provider: "anthropic", id: "claude-fable-5" };

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({ currentModel: model, registryModels: [model] }),
			event: createEvent(),
			config: createConfig({ compactionModel: "anthropic/claude-fable-5" }),
		});

		expect(result).toEqual({
			ok: false,
			reason: "same-as-current-model",
			modelSpec: "anthropic/claude-fable-5",
		});
	});

	test("returns auth-failed when the registry reports an auth error", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({
				currentModel: { provider: "anthropic", id: "claude-fable-5" },
				registryModels: [{ provider: "google", id: "gemini-2.5-flash" }],
				auth: { ok: false, error: "no API key configured" },
			}),
			event: createEvent(),
			config: createConfig({ compactionModel: "google/gemini-2.5-flash" }),
		});

		expect(result).toEqual({
			ok: false,
			reason: "auth-failed",
			modelSpec: "google/gemini-2.5-flash",
			errorMessage: "no API key configured",
		});
	});

	test("runs pi's native compact() with the configured model, auth, and thinking level", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();
		const fallbackModel = { provider: "google", id: "gemini-2.5-flash" };
		const compactCalls: unknown[][] = [];
		const compactionResult = {
			summary: "## Goal\nShip the auth feature.",
			firstKeptEntryId: "entry-keep",
			tokensBefore: 1234,
			details: { readFiles: ["a.ts"], modifiedFiles: [] },
		};

		const event = createEvent();
		const result = await runNativeFallbackCompaction({
			ctx: createCtx({
				currentModel: { provider: "anthropic", id: "claude-fable-5" },
				registryModels: [fallbackModel],
			}),
			event,
			config: createConfig({ compactionModel: "google/gemini-2.5-flash", compactionThinkingLevel: "low" }),
			compactFn: (async (...args: unknown[]) => {
				compactCalls.push(args);
				return compactionResult;
			}) as never,
		});

		expect(result).toEqual({
			ok: true,
			result: compactionResult,
			model: { provider: "google", id: "gemini-2.5-flash" },
		});
		expect(compactCalls.length).toBe(1);
		const [preparation, model, apiKey, headers, customInstructions, signal, thinkingLevel, streamFn, env] =
			compactCalls[0]!;
		expect(preparation).toBe((event as { preparation: unknown }).preparation);
		expect(model).toBe(fallbackModel as never);
		expect(apiKey).toBe("sk-fallback");
		expect(headers).toEqual({ "x-h": "1" });
		expect(customInstructions).toBe("focus on auth work");
		expect(signal).toBe((event as { signal: AbortSignal }).signal);
		expect(thinkingLevel).toBe("low");
		expect(streamFn).toBeUndefined();
		expect(env).toEqual({ E: "1" });
	});

	test("maps abort errors from compact() to the aborted reason", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({
				registryModels: [{ provider: "google", id: "gemini-2.5-flash" }],
			}),
			event: createEvent(),
			config: createConfig({ compactionModel: "google/gemini-2.5-flash" }),
			compactFn: (async () => {
				throw new DOMException("The operation was aborted.", "AbortError");
			}) as never,
		});

		expect(result).toEqual({
			ok: false,
			reason: "aborted",
			modelSpec: "google/gemini-2.5-flash",
		});
	});

	test("maps generic compact() failures to compact-failed with the error message", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({
				registryModels: [{ provider: "google", id: "gemini-2.5-flash" }],
			}),
			event: createEvent(),
			config: createConfig({ compactionModel: "google/gemini-2.5-flash" }),
			compactFn: (async () => {
				throw new Error("Summarization failed: rate limited");
			}) as never,
		});

		expect(result).toEqual({
			ok: false,
			reason: "compact-failed",
			modelSpec: "google/gemini-2.5-flash",
			errorMessage: "Summarization failed: rate limited",
		});
	});

	test("rejects empty summaries so a blank compaction never replaces history", async () => {
		const { runNativeFallbackCompaction } = await loadNativeFallbackModule();

		const result = await runNativeFallbackCompaction({
			ctx: createCtx({
				registryModels: [{ provider: "google", id: "gemini-2.5-flash" }],
			}),
			event: createEvent(),
			config: createConfig({ compactionModel: "google/gemini-2.5-flash" }),
			compactFn: (async () => ({
				summary: "   ",
				firstKeptEntryId: "entry-keep",
				tokensBefore: 1234,
			})) as never,
		});

		expect(result).toEqual({
			ok: false,
			reason: "empty-summary",
			modelSpec: "google/gemini-2.5-flash",
		});
	});
});
