import { writeDebugArtifact } from "./debug";
import type { NativeCompactionRuntime } from "./runtime";
import type { NativeCompactionRequestBody } from "./serializer";
import type { ArtifactContext, ExtensionConfig } from "./types";

const JSON_CONTENT_TYPE = "application/json";

type CompactResponseEnvelope = {
	id?: string;
	created_at?: number | string;
	output: unknown[];
	[key: string]: unknown;
};

export type NativeCompactionClientFailureReason =
	| "aborted"
	| "network-error"
	| "non-2xx"
	| "empty-body"
	| "invalid-json"
	| "malformed-response"
	| "empty-output";

export type NativeCompactionClientSuccess = {
	ok: true;
	status: number;
	compactedWindow: unknown[];
	compactResponseId?: string;
	createdAt?: string;
	/** Assistant summary text extracted from the compact output, for CompactionEntry.summary. */
	summaryText?: string;
	response: CompactResponseEnvelope;
};

export type NativeCompactionClientFailure = {
	ok: false;
	reason: NativeCompactionClientFailureReason;
	status?: number;
	errorMessage?: string;
	responseText?: string;
	responseJson?: unknown;
};

export type NativeCompactionClientResult = NativeCompactionClientSuccess | NativeCompactionClientFailure;

export type ExecuteNativeCompactionOptions = {
	runtime: NativeCompactionRuntime;
	request: NativeCompactionRequestBody;
	signal?: AbortSignal;
	settings?: ExtensionConfig;
	context?: ArtifactContext;
};

function isRecord(value: unknown): value is Record<string, unknown> {
	return !!value && typeof value === "object" && !Array.isArray(value);
}

function isAbortError(error: unknown): boolean {
	return (
		(error instanceof DOMException && error.name === "AbortError") ||
		(error instanceof Error && (error.name === "AbortError" || error.name === "ABORT_ERR"))
	);
}

function normalizeResponseTimestamp(value: unknown): string | undefined {
	if (typeof value === "number" && Number.isFinite(value)) {
		const milliseconds = value > 1_000_000_000_000 ? value : value * 1000;
		return new Date(milliseconds).toISOString();
	}

	if (typeof value !== "string") {
		return undefined;
	}

	const trimmed = value.trim();
	if (!trimmed) {
		return undefined;
	}

	const parsed = Date.parse(trimmed);
	return Number.isNaN(parsed) ? trimmed : new Date(parsed).toISOString();
}

function isCompactOutputItem(value: unknown): value is Record<string, unknown> {
	return isRecord(value);
}

function isCompactResponseEnvelope(value: unknown): value is CompactResponseEnvelope {
	return isRecord(value) && Array.isArray(value.output) && value.output.every(isCompactOutputItem);
}

/**
 * Extract the assistant-authored summary text from the compacted window so the
 * persisted CompactionEntry.summary carries real context. Without this, switching
 * to a non-Responses model later would replay a meaningless placeholder.
 */
export function extractCompactedSummaryText(output: readonly unknown[]): string | undefined {
	const texts: string[] = [];
	for (const item of output) {
		if (!isRecord(item) || item.type !== "message" || item.role !== "assistant" || !Array.isArray(item.content)) {
			continue;
		}
		for (const block of item.content) {
			if (isRecord(block) && block.type === "output_text" && typeof block.text === "string" && block.text.trim()) {
				texts.push(block.text.trim());
			}
		}
	}

	const joined = texts.join("\n\n").trim();
	return joined.length > 0 ? joined : undefined;
}

function decodeJwtPayload(token: string): Record<string, unknown> | undefined {
	const parts = token.split(".");
	if (parts.length !== 3) {
		return undefined;
	}

	try {
		const payloadText = Buffer.from(parts[1]!, "base64url").toString("utf8");
		const payload = JSON.parse(payloadText);
		return isRecord(payload) ? payload : undefined;
	} catch {
		return undefined;
	}
}

function extractCodexAccountId(token: string): string | undefined {
	const payload = decodeJwtPayload(token);
	const authClaims = payload?.["https://api.openai.com/auth"];
	if (!isRecord(authClaims)) {
		return undefined;
	}

	const accountId = authClaims.chatgpt_account_id;
	return typeof accountId === "string" && accountId.trim().length > 0 ? accountId.trim() : undefined;
}

function buildCodexUserAgent(): string {
	const platform = typeof process !== "undefined" ? process.platform : "browser";
	const arch = typeof process !== "undefined" ? process.arch : "unknown";
	return `pi (${platform}; ${arch})`;
}

function toHeaders(runtime: NativeCompactionRuntime): Record<string, string> {
	const headers = new Headers(runtime.currentModel.headers ?? {});
	for (const [key, value] of Object.entries(runtime.headers ?? {})) {
		headers.set(key, value);
	}
	headers.set("accept", JSON_CONTENT_TYPE);
	headers.set("content-type", JSON_CONTENT_TYPE);
	if (!headers.has("authorization")) {
		headers.set("authorization", `Bearer ${runtime.apiKey}`);
	}

	if (runtime.api === "openai-codex-responses") {
		const accountId = extractCodexAccountId(runtime.apiKey);
		if (accountId) {
			headers.set("chatgpt-account-id", accountId);
		}
		headers.set("originator", "pi");
		headers.set("user-agent", buildCodexUserAgent());
		headers.set("openai-beta", "responses=experimental");
	}

	return Object.fromEntries(headers.entries());
}

function writeCompactArtifact(
	data: unknown,
	settings: ExtensionConfig | undefined,
	context: ArtifactContext | undefined,
): void {
	if (!settings || !context) {
		return;
	}

	writeDebugArtifact("compact-response", data, settings, context);
}

export async function executeNativeCompaction(
	options: ExecuteNativeCompactionOptions,
): Promise<NativeCompactionClientResult> {
	const { runtime, request, signal, settings, context } = options;
	const headers = toHeaders(runtime);

	if (signal?.aborted) {
		const aborted: NativeCompactionClientFailure = {
			ok: false,
			reason: "aborted",
		};
		writeCompactArtifact(
			{
				request: {
					url: runtime.compactUrl,
					headers,
					body: request,
				},
				outcome: aborted,
			},
			settings,
			context,
		);
		return aborted;
	}

	try {
		const response = await fetch(runtime.compactUrl, {
			method: "POST",
			headers,
			body: JSON.stringify(request),
			signal,
		});
		const responseText = await response.text();
		const responseHeaders: Record<string, string> = {};
		response.headers.forEach((value, key) => {
			responseHeaders[key] = value;
		});

		if (!response.ok) {
			let responseJson: unknown;
			if (responseText.trim().length > 0) {
				try {
					responseJson = JSON.parse(responseText);
				} catch {
					responseJson = undefined;
				}
			}

			const failure: NativeCompactionClientFailure = {
				ok: false,
				reason: "non-2xx",
				status: response.status,
				responseText: responseText || undefined,
				responseJson,
			};
			writeCompactArtifact(
				{
					request: {
						url: runtime.compactUrl,
						headers,
						body: request,
					},
					response: {
						status: response.status,
						headers: responseHeaders,
						body: responseJson ?? responseText,
					},
					outcome: failure,
				},
				settings,
				context,
			);
			return failure;
		}

		if (!responseText.trim()) {
			const failure: NativeCompactionClientFailure = {
				ok: false,
				reason: "empty-body",
				status: response.status,
			};
			writeCompactArtifact(
				{
					request: {
						url: runtime.compactUrl,
						headers,
						body: request,
					},
					response: {
						status: response.status,
						headers: responseHeaders,
						body: responseText,
					},
					outcome: failure,
				},
				settings,
				context,
			);
			return failure;
		}

		let parsed: unknown;
		try {
			parsed = JSON.parse(responseText);
		} catch (error) {
			const failure: NativeCompactionClientFailure = {
				ok: false,
				reason: "invalid-json",
				status: response.status,
				errorMessage: error instanceof Error ? error.message : String(error),
				responseText,
			};
			writeCompactArtifact(
				{
					request: {
						url: runtime.compactUrl,
						headers,
						body: request,
					},
					response: {
						status: response.status,
						headers: responseHeaders,
						body: responseText,
					},
					outcome: failure,
				},
				settings,
				context,
			);
			return failure;
		}

		if (!isCompactResponseEnvelope(parsed)) {
			const failure: NativeCompactionClientFailure = {
				ok: false,
				reason: "malformed-response",
				status: response.status,
				responseJson: parsed,
			};
			writeCompactArtifact(
				{
					request: {
						url: runtime.compactUrl,
						headers,
						body: request,
					},
					response: {
						status: response.status,
						headers: responseHeaders,
						body: parsed,
					},
					outcome: failure,
				},
				settings,
				context,
			);
			return failure;
		}

		if (parsed.output.length === 0) {
			const failure: NativeCompactionClientFailure = {
				ok: false,
				reason: "empty-output",
				status: response.status,
				responseJson: parsed,
			};
			writeCompactArtifact(
				{
					request: {
						url: runtime.compactUrl,
						headers,
						body: request,
					},
					response: {
						status: response.status,
						headers: responseHeaders,
						body: parsed,
					},
					outcome: failure,
				},
				settings,
				context,
			);
			return failure;
		}

		const success: NativeCompactionClientSuccess = {
			ok: true,
			status: response.status,
			compactedWindow: [...parsed.output],
			compactResponseId: typeof parsed.id === "string" && parsed.id.trim() ? parsed.id.trim() : undefined,
			createdAt: normalizeResponseTimestamp(parsed.created_at),
			summaryText: extractCompactedSummaryText(parsed.output),
			response: parsed,
		};
		writeCompactArtifact(
			{
				request: {
					url: runtime.compactUrl,
					headers,
					body: request,
				},
				response: {
					status: response.status,
					headers: responseHeaders,
					body: parsed,
				},
				outcome: {
					ok: true,
					status: success.status,
					compactResponseId: success.compactResponseId,
					createdAt: success.createdAt,
					compactedItems: success.compactedWindow.length,
				},
			},
			settings,
			context,
		);
		return success;
	} catch (error) {
		const failure: NativeCompactionClientFailure = isAbortError(error)
			? {
				ok: false,
				reason: "aborted",
			}
			: {
				ok: false,
				reason: "network-error",
				errorMessage: error instanceof Error ? error.message : String(error),
			};

		writeCompactArtifact(
			{
				request: {
					url: runtime.compactUrl,
					headers,
					body: request,
				},
				outcome: failure,
			},
			settings,
			context,
		);
		return failure;
	}
}
