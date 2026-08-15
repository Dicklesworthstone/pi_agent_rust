import { beforeEach, describe, expect, test } from "bun:test";
import {
	clearRequestContextCache,
	getCompactionRequestExtras,
	rememberRequestContext,
} from "./request-context-cache";

beforeEach(() => {
	clearRequestContextCache();
});

describe("request context cache", () => {
	test("captures only compact-relevant fields from a live payload", () => {
		rememberRequestContext(
			{
				model: "gpt-5-mini",
				input: [{ role: "user", content: [] }],
				instructions: "system prompt",
				stream: true,
				store: false,
				include: ["reasoning.encrypted_content"],
				tool_choice: "auto",
				tools: [{ type: "function", name: "read" }],
				parallel_tool_calls: true,
				reasoning: { effort: "high", summary: "auto" },
				service_tier: "flex",
				prompt_cache_key: "session-1",
				text: { verbosity: "low" },
			},
			"session-1",
		);

		const extras = getCompactionRequestExtras("gpt-5-mini", "session-1");
		expect(extras).toEqual({
			tools: [{ type: "function", name: "read" }],
			parallel_tool_calls: true,
			reasoning: { effort: "high", summary: "auto" },
			service_tier: "flex",
			prompt_cache_key: "session-1",
			text: { verbosity: "low" },
		});
		// stream/store/include/tool_choice must never leak into the compact request.
		expect(extras && "stream" in extras).toBe(false);
		expect(extras && "tool_choice" in extras).toBe(false);
	});

	test("misses when the model differs", () => {
		rememberRequestContext({ model: "gpt-5-mini", input: [], reasoning: { effort: "low" } }, "session-1");

		expect(getCompactionRequestExtras("gpt-5.1", "session-1")).toBeUndefined();
	});

	test("misses when the session differs", () => {
		rememberRequestContext({ model: "gpt-5-mini", input: [], reasoning: { effort: "low" } }, "session-1");

		expect(getCompactionRequestExtras("gpt-5-mini", "session-2")).toBeUndefined();
	});

	test("matches when either side has no session id", () => {
		rememberRequestContext({ model: "gpt-5-mini", input: [], parallel_tool_calls: false });

		expect(getCompactionRequestExtras("gpt-5-mini", "session-1")).toEqual({ parallel_tool_calls: false });
		expect(getCompactionRequestExtras("gpt-5-mini")).toEqual({ parallel_tool_calls: false });
	});

	test("returns isolated clones so later mutation cannot corrupt the cache", () => {
		rememberRequestContext({ model: "gpt-5-mini", input: [], tools: [{ name: "read" }] });

		const first = getCompactionRequestExtras("gpt-5-mini");
		(first!.tools![0] as Record<string, unknown>).name = "mutated";

		expect(getCompactionRequestExtras("gpt-5-mini")).toEqual({ tools: [{ name: "read" }] });
	});

	test("empty extras are still valid (payload had none of the fields)", () => {
		rememberRequestContext({ model: "gpt-5-mini", input: [] });

		expect(getCompactionRequestExtras("gpt-5-mini")).toEqual({});
	});

	test("clearRequestContextCache empties the cache", () => {
		rememberRequestContext({ model: "gpt-5-mini", input: [], parallel_tool_calls: true });
		clearRequestContextCache();

		expect(getCompactionRequestExtras("gpt-5-mini")).toBeUndefined();
	});
});
