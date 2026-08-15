import {
	compact,
	type CompactionResult,
	type ExtensionContext,
	type SessionBeforeCompactEvent,
} from "@earendil-works/pi-coding-agent";
import type { ExtensionConfig } from "./types";

export type ParsedModelSpec = {
	provider: string;
	modelId: string;
};

export type NativeFallbackFailureReason =
	| "no-model-configured"
	| "invalid-model-spec"
	| "model-not-found"
	| "same-as-current-model"
	| "auth-failed"
	| "aborted"
	| "empty-summary"
	| "compact-failed";

export type NativeFallbackResult =
	| {
			ok: true;
			result: CompactionResult;
			model: { provider: string; id: string };
	  }
	| {
			ok: false;
			reason: NativeFallbackFailureReason;
			modelSpec?: string;
			errorMessage?: string;
	  };

/** pi's exported native compact(); injectable for tests. */
export type NativeCompactFn = typeof compact;

type ResolvedAuth =
	| { ok: true; apiKey?: string; headers?: Record<string, string>; env?: Record<string, string> }
	| { ok: false; error: string };

/** Parse "provider/model-id" (model ids may themselves contain slashes). */
export function parseModelSpec(spec: string): ParsedModelSpec | undefined {
	const trimmed = spec.trim();
	const separatorIndex = trimmed.indexOf("/");
	if (separatorIndex <= 0 || separatorIndex >= trimmed.length - 1) {
		return undefined;
	}

	const provider = trimmed.slice(0, separatorIndex).trim();
	const modelId = trimmed.slice(separatorIndex + 1).trim();
	if (!provider || !modelId) {
		return undefined;
	}

	return { provider, modelId };
}

function isAbortError(error: unknown): boolean {
	return (
		(error instanceof DOMException && error.name === "AbortError") ||
		(error instanceof Error && (error.name === "AbortError" || error.name === "ABORT_ERR"))
	);
}

function toErrorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

/**
 * Run pi's native compaction method with the user-configured compaction model.
 *
 * Only handles the "configured model differs from the current one" case: when no model
 * is configured (or it equals the current model), the caller should return undefined from
 * session_before_compact so pi runs the same native path itself, keeping its internal
 * streamFn/thinkingLevel wiring.
 */
export async function runNativeFallbackCompaction(args: {
	ctx: ExtensionContext;
	event: SessionBeforeCompactEvent;
	config: ExtensionConfig;
	compactFn?: NativeCompactFn;
}): Promise<NativeFallbackResult> {
	const { ctx, event, config } = args;
	const compactFn = args.compactFn ?? compact;

	const spec = config.compactionModel?.trim();
	if (!spec) {
		return { ok: false, reason: "no-model-configured" };
	}

	const parsed = parseModelSpec(spec);
	if (!parsed) {
		return { ok: false, reason: "invalid-model-spec", modelSpec: spec };
	}

	const model = ctx.modelRegistry.find(parsed.provider, parsed.modelId);
	if (!model) {
		return { ok: false, reason: "model-not-found", modelSpec: spec };
	}

	if (ctx.model && ctx.model.provider === model.provider && ctx.model.id === model.id) {
		return { ok: false, reason: "same-as-current-model", modelSpec: spec };
	}

	let auth: ResolvedAuth;
	try {
		auth = (await ctx.modelRegistry.getApiKeyAndHeaders(model)) as ResolvedAuth;
	} catch (error) {
		return { ok: false, reason: "auth-failed", modelSpec: spec, errorMessage: toErrorMessage(error) };
	}
	if (!auth.ok) {
		return { ok: false, reason: "auth-failed", modelSpec: spec, errorMessage: auth.error };
	}

	try {
		const result = await compactFn(
			event.preparation,
			model,
			auth.apiKey,
			auth.headers,
			event.customInstructions,
			event.signal,
			config.compactionThinkingLevel,
			undefined,
			auth.env,
		);

		if (event.signal.aborted) {
			return { ok: false, reason: "aborted", modelSpec: spec };
		}
		if (!result.summary || result.summary.trim().length === 0) {
			return { ok: false, reason: "empty-summary", modelSpec: spec };
		}

		return {
			ok: true,
			result,
			model: { provider: model.provider, id: model.id },
		};
	} catch (error) {
		if (event.signal.aborted || isAbortError(error)) {
			return { ok: false, reason: "aborted", modelSpec: spec };
		}
		return { ok: false, reason: "compact-failed", modelSpec: spec, errorMessage: toErrorMessage(error) };
	}
}
