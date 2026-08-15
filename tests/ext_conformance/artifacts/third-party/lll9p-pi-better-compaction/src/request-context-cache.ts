import type { ResponsesCompatibleRequestPayload } from "./runtime";

/**
 * Fields the latest codex_rs CompactionInput accepts beyond model/input/instructions.
 * The compact endpoint has no payload of its own at session_before_compact time, so we
 * mirror them from the most recent live provider request for the same model/session.
 */
export type CompactionRequestExtras = {
	tools?: unknown[];
	parallel_tool_calls?: boolean;
	reasoning?: Record<string, unknown>;
	service_tier?: string;
	prompt_cache_key?: string;
	text?: Record<string, unknown>;
};

type CachedRequestContext = {
	model: string;
	sessionId?: string;
	extras: CompactionRequestExtras;
};

let cached: CachedRequestContext | undefined;

function isRecord(value: unknown): value is Record<string, unknown> {
	return !!value && typeof value === "object" && !Array.isArray(value);
}

/**
 * Remember compact-relevant fields from a live Responses request payload.
 * Purely additive metadata: failures are swallowed so caching can never
 * break the provider request path.
 */
export function rememberRequestContext(payload: ResponsesCompatibleRequestPayload, sessionId?: string): void {
	try {
		const extras: CompactionRequestExtras = {};
		if (Array.isArray(payload.tools)) {
			extras.tools = structuredClone(payload.tools);
		}
		if (typeof payload.parallel_tool_calls === "boolean") {
			extras.parallel_tool_calls = payload.parallel_tool_calls;
		}
		if (isRecord(payload.reasoning)) {
			extras.reasoning = structuredClone(payload.reasoning);
		}
		if (typeof payload.service_tier === "string" && payload.service_tier.trim().length > 0) {
			extras.service_tier = payload.service_tier;
		}
		if (typeof payload.prompt_cache_key === "string" && payload.prompt_cache_key.trim().length > 0) {
			extras.prompt_cache_key = payload.prompt_cache_key;
		}
		if (isRecord(payload.text)) {
			extras.text = structuredClone(payload.text);
		}

		cached = {
			model: payload.model,
			sessionId,
			extras,
		};
	} catch {
		cached = undefined;
	}
}

/** Return cached extras when they were captured for the same model (and session, when known). */
export function getCompactionRequestExtras(model: string, sessionId?: string): CompactionRequestExtras | undefined {
	if (!cached || cached.model !== model) {
		return undefined;
	}
	if (cached.sessionId !== undefined && sessionId !== undefined && cached.sessionId !== sessionId) {
		return undefined;
	}

	try {
		return structuredClone(cached.extras);
	} catch {
		return undefined;
	}
}

export function clearRequestContextCache(): void {
	cached = undefined;
}
