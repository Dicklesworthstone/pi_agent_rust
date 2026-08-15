import { afterEach, expect, mock, test } from "bun:test";
import { executeNativeCompaction, extractCompactedSummaryText } from "./compact-client";
import { buildCompactUrl } from "./runtime";

const baseModel = {
	provider: "openai",
	api: "openai-responses",
	id: "gpt-5-mini",
	name: "gpt-5-mini",
	baseUrl: "https://api.openai.com/v1",
	reasoning: true,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 100000,
	maxTokens: 1000,
};

let serializerImportCounter = 0;

async function loadSerializerModule() {
	mock.module("@earendil-works/pi-coding-agent", () => ({
		convertToLlm: (messages: unknown[]) => messages,
	}));
	return import(`./serializer.ts?unit=${serializerImportCounter++}`);
}

function createJwtWithAccountId(accountId: string): string {
	const header = Buffer.from(JSON.stringify({ alg: "none", typ: "JWT" })).toString("base64url");
	const payload = Buffer.from(
		JSON.stringify({
			"https://api.openai.com/auth": {
				chatgpt_account_id: accountId,
			},
		}),
	).toString("base64url");
	return `${header}.${payload}.signature`;
}

afterEach(() => {
	serializerImportCounter = 0;
	mock.restore();
});

test("buildCompactUrl uses codex compact path for openai-codex responses", () => {
	expect(buildCompactUrl("https://api.openai.com/v1", "openai-responses")).toBe(
		"https://api.openai.com/v1/responses/compact",
	);
	expect(buildCompactUrl("https://chatgpt.com/backend-api", "openai-codex-responses")).toBe(
		"https://chatgpt.com/backend-api/codex/responses/compact",
	);
	expect(buildCompactUrl("https://chatgpt.com/backend-api/codex", "openai-codex-responses")).toBe(
		"https://chatgpt.com/backend-api/codex/responses/compact",
	);
	expect(buildCompactUrl("https://chatgpt.com/backend-api/codex/responses", "openai-codex-responses")).toBe(
		"https://chatgpt.com/backend-api/codex/responses/compact",
	);
});

test("executeNativeCompaction propagates resolved request headers and codex auth headers", async () => {
	const token = createJwtWithAccountId("acct_123");
	let fetchArgs: { url?: string; init?: RequestInit } = {};
	globalThis.fetch = mock(async (url: string | URL | Request, init?: RequestInit) => {
		fetchArgs = { url: String(url), init };
		return new Response(JSON.stringify({ output: [{ type: "compaction", encrypted_content: "opaque" }] }), {
			status: 200,
			headers: { "content-type": "application/json" },
		});
	}) as typeof fetch;

	const result = await executeNativeCompaction({
		runtime: {
			provider: "openai-codex",
			api: "openai-codex-responses",
			model: "gpt-5.1",
			baseUrl: "https://chatgpt.com/backend-api",
			apiKey: token,
			headers: {
				"x-test-model-header": "present",
				"x-test-runtime-header": "resolved",
			},
			compactPath: "codex/responses/compact",
			compactUrl: buildCompactUrl("https://chatgpt.com/backend-api", "openai-codex-responses"),
			currentModel: {
				...baseModel,
				provider: "openai-codex",
				api: "openai-codex-responses",
				id: "gpt-5.1",
				name: "gpt-5.1",
				baseUrl: "https://chatgpt.com/backend-api",
			},
		},
		request: {
			model: "gpt-5.1",
			instructions: "compact this",
			input: [{ role: "user", content: [{ type: "input_text", text: "hello" }] }],
		},
	});

	expect(result.ok).toBe(true);
	expect(fetchArgs.url).toBe("https://chatgpt.com/backend-api/codex/responses/compact");
	const headers = new Headers(fetchArgs.init?.headers);
	expect(headers.get("x-test-model-header")).toBe("present");
	expect(headers.get("x-test-runtime-header")).toBe("resolved");
	expect(headers.get("authorization")).toBe(`Bearer ${token}`);
	expect(headers.get("chatgpt-account-id")).toBe("acct_123");
	expect(headers.get("originator")).toBe("pi");
	expect(headers.get("openai-beta")).toBe("responses=experimental");
	expect(headers.get("content-type")).toBe("application/json");
});

test("serializer sanitizes unpaired surrogates in instructions and message content", async () => {
	const { serializeMessagesToCompactRequest, serializeMessagesToResponsesInput } = await loadSerializerModule();
	const invalid = "\ud800Hello\udc00";
	const request = serializeMessagesToCompactRequest({
		model: baseModel as never,
		instructions: `Prefix ${invalid}`,
		messages: [
			{ role: "user", content: [{ type: "text", text: invalid }], timestamp: 1 },
			{
				role: "assistant",
				provider: baseModel.provider,
				api: baseModel.api,
				model: baseModel.id,
				stopReason: "stop",
				content: [{ type: "text", text: invalid, textSignature: JSON.stringify({ v: 1, id: "msg_1" }) }],
				timestamp: 2,
			},
			{
				role: "toolResult",
				toolCallId: "call_1|fc_call_1",
				toolName: "read",
				isError: false,
				content: [{ type: "text", text: invalid }],
				timestamp: 3,
			},
		],
	});

	expect(JSON.stringify(request.instructions)).not.toContain("\\ud800");
	expect(JSON.stringify(request.input)).not.toContain("\\ud800");
	expect(JSON.stringify(request.input)).not.toContain("\\udc00");

	const inputOnly = serializeMessagesToResponsesInput(baseModel as never, [
		{ role: "user", content: [{ type: "text", text: invalid }], timestamp: 1 },
	] as never);
	expect(JSON.stringify(inputOnly)).not.toContain("\\ud800");
	expect(JSON.stringify(inputOnly)).not.toContain("\\udc00");
});

test("extractCompactedSummaryText joins assistant output_text blocks and skips opaque items", () => {
	expect(
		extractCompactedSummaryText([
			{ type: "compaction", encrypted_content: "opaque" },
			{
				type: "message",
				role: "assistant",
				status: "completed",
				id: "cmp_1",
				content: [
					{ type: "output_text", text: "  Summary part one.  ", annotations: [] },
					{ type: "output_text", text: "Summary part two.", annotations: [] },
				],
			},
			{ type: "message", role: "user", content: [{ type: "input_text", text: "not a summary" }] },
		]),
	).toBe("Summary part one.\n\nSummary part two.");

	expect(extractCompactedSummaryText([{ type: "compaction", encrypted_content: "opaque" }])).toBeUndefined();
	expect(
		extractCompactedSummaryText([
			{ type: "message", role: "assistant", status: "completed", id: "cmp_2", content: [] },
		]),
	).toBeUndefined();
});

test("executeNativeCompaction serializes codex-aligned passthrough fields and extracts summary text", async () => {
	let requestBody: Record<string, unknown> = {};
	globalThis.fetch = mock(async (_url: string | URL | Request, init?: RequestInit) => {
		requestBody = JSON.parse(String(init?.body));
		return new Response(
			JSON.stringify({
				id: "resp_compact",
				output: [
					{ type: "compaction", encrypted_content: "opaque" },
					{
						type: "message",
						role: "assistant",
						status: "completed",
						id: "cmp_summary",
						content: [{ type: "output_text", text: "Native summary.", annotations: [] }],
					},
				],
			}),
			{ status: 200, headers: { "content-type": "application/json" } },
		);
	}) as typeof fetch;

	const result = await executeNativeCompaction({
		runtime: {
			provider: "openai",
			api: "openai-responses",
			model: "gpt-5-mini",
			baseUrl: "https://api.openai.com/v1",
			apiKey: "sk-test",
			compactPath: "responses/compact",
			compactUrl: buildCompactUrl("https://api.openai.com/v1", "openai-responses"),
			currentModel: baseModel as never,
		},
		request: {
			model: "gpt-5-mini",
			instructions: "compact this",
			input: [{ role: "user", content: [{ type: "input_text", text: "hello" }] }],
			tools: [{ type: "function", name: "read" }],
			parallel_tool_calls: true,
			reasoning: { effort: "high", summary: "auto" },
			prompt_cache_key: "session-123",
			text: { verbosity: "low" },
		},
	});

	expect(result.ok).toBe(true);
	if (result.ok) {
		expect(result.summaryText).toBe("Native summary.");
	}
	expect(requestBody.tools).toEqual([{ type: "function", name: "read" }]);
	expect(requestBody.parallel_tool_calls).toBe(true);
	expect(requestBody.reasoning).toEqual({ effort: "high", summary: "auto" });
	expect(requestBody.prompt_cache_key).toBe("session-123");
	expect(requestBody.text).toEqual({ verbosity: "low" });
	expect("service_tier" in requestBody).toBe(false);
});
