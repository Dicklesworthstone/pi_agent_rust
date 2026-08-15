import { describe, expect, test } from "bun:test";
import { resolveNativeCompactionEnvironment } from "../src/runtime";

describe("resolveNativeCompactionEnvironment", () => {
	test("uses getApiKeyAndHeaders to resolve request auth", async () => {
		const resolution = await resolveNativeCompactionEnvironment({
			model: {
				provider: "openai",
				api: "openai-responses",
				id: "gpt-5.4",
				baseUrl: "https://example.com/v1",
			},
			modelRegistry: {
				async getApiKeyAndHeaders(model: { provider: string; id: string }) {
					if (model.provider !== "openai" || model.id !== "gpt-5.4") {
						return { ok: false, error: "unexpected model" };
					}
					return {
						ok: true,
						apiKey: "sk-openai",
						headers: {
							"x-test-request-header": "present",
						},
					};
				},
			},
		} as any);

		expect(resolution).toEqual({
			ok: true,
			runtime: expect.objectContaining({
				provider: "openai",
				api: "openai-responses",
				model: "gpt-5.4",
				baseUrl: "https://example.com/v1",
				apiKey: "sk-openai",
				headers: {
					"x-test-request-header": "present",
				},
				compactPath: "responses/compact",
				compactUrl: "https://example.com/v1/responses/compact",
			}),
		});
	});

	test("returns missing-api-key when request auth resolves without an api key", async () => {
		const resolution = await resolveNativeCompactionEnvironment({
			model: {
				provider: "openai",
				api: "openai-responses",
				id: "gpt-5.4",
				baseUrl: "https://example.com/v1",
			},
			modelRegistry: {
				async getApiKeyAndHeaders() {
					return {
						ok: true,
						apiKey: undefined,
						headers: {
							"x-test-request-header": "present",
						},
					};
				},
			},
		} as any);

		expect(resolution).toEqual({
			ok: false,
			reason: "missing-api-key",
			provider: "openai",
			api: "openai-responses",
			model: "gpt-5.4",
			baseUrl: "https://example.com/v1",
		});
	});

	test("selects by API family: any provider speaking openai-responses qualifies by default", async () => {
		const resolution = await resolveNativeCompactionEnvironment({
			model: {
				provider: "custom-litellm",
				api: "openai-responses",
				id: "gpt-5.4",
				baseUrl: "https://proxy.example.com/v1",
			},
			modelRegistry: {
				async getApiKeyAndHeaders(model: { provider: string; id: string }) {
					if (model.provider !== "custom-litellm" || model.id !== "gpt-5.4") {
						return { ok: false, error: "unexpected model" };
					}
					return {
						ok: true,
						apiKey: "sk-custom-litellm",
						headers: {
							"x-proxy-header": "proxy-value",
						},
					};
				},
			},
		} as any);

		expect(resolution).toEqual({
			ok: true,
			runtime: expect.objectContaining({
				provider: "custom-litellm",
				api: "openai-responses",
				model: "gpt-5.4",
				baseUrl: "https://proxy.example.com/v1",
				apiKey: "sk-custom-litellm",
				headers: {
					"x-proxy-header": "proxy-value",
				},
				compactPath: "responses/compact",
				compactUrl: "https://proxy.example.com/v1/responses/compact",
			}),
		});
	});

	test("rejects non-Responses APIs so they take the native-method fallback path", async () => {
		const resolution = await resolveNativeCompactionEnvironment({
			model: {
				provider: "anthropic",
				api: "anthropic-messages",
				id: "claude-fable-5",
				baseUrl: "https://api.anthropic.com",
			},
			modelRegistry: {
				async getApiKeyAndHeaders() {
					return { ok: true, apiKey: "sk-ant" };
				},
			},
		} as any);

		expect(resolution).toEqual({
			ok: false,
			reason: "unsupported-api",
			provider: "anthropic",
			api: "anthropic-messages",
			model: "claude-fable-5",
			baseUrl: "https://api.anthropic.com",
		});
	});

	test("honors responsesCompactApis narrowing from config", async () => {
		const resolution = await resolveNativeCompactionEnvironment(
			{
				model: {
					provider: "openai",
					api: "openai-responses",
					id: "gpt-5.4",
					baseUrl: "https://example.com/v1",
				},
				modelRegistry: {
					async getApiKeyAndHeaders() {
						return { ok: true, apiKey: "sk-openai" };
					},
				},
			} as any,
			{
				responsesCompactApis: ["openai-codex-responses"],
			},
		);

		expect(resolution).toEqual({
			ok: false,
			reason: "unsupported-api",
			provider: "openai",
			api: "openai-responses",
			model: "gpt-5.4",
			baseUrl: "https://example.com/v1",
		});
	});
});
