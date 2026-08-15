import type {
	BeforeProviderRequestEvent,
	CompactionResult,
	ExtensionAPI,
	ExtensionContext,
	SessionBeforeCompactEvent,
} from "@earendil-works/pi-coding-agent";
import { executeNativeCompaction } from "./compact-client";
import { loadExtensionConfig } from "./config";
import { writeDebugArtifact } from "./debug";
import { resolveLatestNativeCompactionEntry } from "./details-store";
import { runNativeFallbackCompaction } from "./native-fallback";
import {
	rewriteResponsesPayloadWithNativeReplay,
	serializeLiveTailToResponsesInput,
} from "./payload-rewrite";
import { getCompactionRequestExtras, rememberRequestContext } from "./request-context-cache";
import {
	isResponsesCompatiblePayload,
	resolveNativeCompactionEnvironment,
	type NativeCompactionRuntime,
} from "./runtime";
import { serializeMessagesToCompactRequest, type NativeCompactionRequestBody, type ResponsesInputItem } from "./serializer";
import {
	createNativeCompactionDetails,
	createNativeCompactionResult,
	EXTENSION_ID,
	isNativeCompactionDetails,
	type ExtensionConfig,
	type NativeCompactionDetails,
	type NativeCompactionRequestMeta,
} from "./types";

type ResponsesCompactOutcome =
	| { outcome: "success"; compaction: CompactionResult<NativeCompactionDetails> }
	| { outcome: "aborted" }
	| { outcome: "failed" };

function buildCompactionRequestMeta(event: SessionBeforeCompactEvent): NativeCompactionRequestMeta {
	return {
		tokensBefore: event.preparation.tokensBefore,
		previousSummaryPresent: Boolean(event.preparation.previousSummary),
	};
}

function getCurrentModelDebugInfo(ctx: ExtensionContext) {
	return ctx.model
		? {
			provider: ctx.model.provider,
			id: ctx.model.id,
		}
		: undefined;
}

function getCompactionIdentityDebugInfo(entry: { details?: unknown } | undefined) {
	return isNativeCompactionDetails(entry?.details)
		? {
			provider: entry.details.provider,
			api: entry.details.api,
			model: entry.details.model,
			baseUrl: entry.details.baseUrl,
		}
		: undefined;
}

function getSessionId(ctx: ExtensionContext): string | undefined {
	try {
		return ctx.sessionManager.getSessionId();
	} catch {
		return undefined;
	}
}

function notifyWarning(ctx: ExtensionContext, message: string): void {
	if (ctx.hasUI) {
		ctx.ui.notify(`${EXTENSION_ID}: ${message}`, "warning");
	}
}

function cloneOpaqueWindow(window: readonly unknown[]): unknown[] {
	return window.map((item) => structuredClone(item));
}

function buildCompactionInstructions(systemPrompt: string, customInstructions?: string): string {
	const guidance = customInstructions?.trim();
	if (!guidance) {
		return systemPrompt;
	}

	return `${systemPrompt}\n\nAdditional user guidance for this manual /compact request:\n${guidance}`;
}

async function runResponsesNativeCompact(
	event: SessionBeforeCompactEvent,
	ctx: ExtensionContext,
	config: ExtensionConfig,
	runtime: NativeCompactionRuntime,
): Promise<ResponsesCompactOutcome> {
	const instructions = buildCompactionInstructions(ctx.getSystemPrompt(), event.customInstructions);
	const branchEntries = ctx.sessionManager.getBranch();
	const latestNativeCompaction = resolveLatestNativeCompactionEntry(branchEntries, {
		provider: runtime.provider,
		api: runtime.api,
		model: runtime.model,
		baseUrl: runtime.baseUrl,
	});

	let requestSource: "session-context" | "non-native-session-context" | "latest-native-replay";
	let request: NativeCompactionRequestBody;
	if (latestNativeCompaction.ok) {
		const liveTailEntries = branchEntries.slice(latestNativeCompaction.index + 1);
		requestSource = "latest-native-replay";
		const input: ResponsesInputItem[] = [
			...(cloneOpaqueWindow(latestNativeCompaction.entry.details.compactedWindow) as ResponsesInputItem[]),
			...serializeLiveTailToResponsesInput({ model: runtime.currentModel, entries: liveTailEntries }),
		];
		request = {
			model: runtime.currentModel.id,
			input,
			instructions,
		};
	} else if (
		latestNativeCompaction.reason === "no-compaction" ||
		(latestNativeCompaction.reason === "latest-compaction-not-native" &&
			config.allowCompactionContinuityBreak)
	) {
		requestSource =
			latestNativeCompaction.reason === "no-compaction" ? "session-context" : "non-native-session-context";
		request = serializeMessagesToCompactRequest({
			model: runtime.currentModel,
			messages: ctx.sessionManager.buildSessionContext().messages,
			instructions,
		});
	} else {
		writeDebugArtifact(
			"compaction-event",
			{
				event: "session_before_compact.responses-compact-skip",
				reason: latestNativeCompaction.reason,
				provider: runtime.provider,
				api: runtime.api,
				model: runtime.model,
				baseUrl: runtime.baseUrl,
				latestCompactionIndex: latestNativeCompaction.latestCompactionIndex,
				latestCompactionIdentity: getCompactionIdentityDebugInfo(latestNativeCompaction.latestCompaction),
			},
			config,
			ctx,
		);
		return { outcome: "failed" };
	}

	// Mirror the latest codex_rs CompactionInput fields captured from the most
	// recent live provider request for this model (tools, reasoning, etc.).
	const extras = getCompactionRequestExtras(runtime.model, getSessionId(ctx));
	if (extras) {
		request = { ...request, ...extras };
	}

	const compactResult = await executeNativeCompaction({
		runtime,
		request,
		signal: event.signal,
		settings: config,
		context: ctx,
	});

	if (compactResult.ok === false) {
		writeDebugArtifact(
			"compaction-event",
			{
				event: "session_before_compact.responses-compact-failure",
				reason: compactResult.reason,
				status: compactResult.status,
				errorMessage: compactResult.errorMessage,
			},
			config,
			ctx,
		);
		return compactResult.reason === "aborted" ? { outcome: "aborted" } : { outcome: "failed" };
	}

	let details: NativeCompactionDetails;
	try {
		details = createNativeCompactionDetails({
			provider: runtime.provider,
			api: runtime.api,
			model: runtime.model,
			baseUrl: runtime.baseUrl,
			compactedWindow: compactResult.compactedWindow,
			compactResponseId: compactResult.compactResponseId,
			createdAt: compactResult.createdAt,
			requestMeta: buildCompactionRequestMeta(event),
		});
	} catch (error) {
		writeDebugArtifact(
			"compaction-event",
			{
				event: "session_before_compact.invalid-native-details",
				reason: error instanceof Error ? error.message : String(error),
				provider: runtime.provider,
				api: runtime.api,
				model: runtime.model,
				baseUrl: runtime.baseUrl,
			},
			config,
			ctx,
		);
		return { outcome: "failed" };
	}

	const compaction = createNativeCompactionResult({
		firstKeptEntryId: event.preparation.firstKeptEntryId,
		tokensBefore: event.preparation.tokensBefore,
		details,
		summary: compactResult.summaryText,
	});

	writeDebugArtifact(
		"compaction-event",
		{
			event: "session_before_compact.responses-compact-success",
			provider: runtime.provider,
			api: runtime.api,
			model: runtime.model,
			requestSource,
			requestInputItems: request.input.length,
			requestExtras: extras ? Object.keys(extras) : [],
			compactResponseId: compactResult.compactResponseId,
			compactedItems: compactResult.compactedWindow.length,
			summaryExtracted: Boolean(compactResult.summaryText),
			firstKeptEntryId: event.preparation.firstKeptEntryId,
		},
		config,
		ctx,
	);

	return { outcome: "success", compaction };
}

async function handleSessionBeforeCompact(event: SessionBeforeCompactEvent, ctx: ExtensionContext) {
	const { config } = loadExtensionConfig();
	if (!config.enabled) {
		return undefined;
	}

	writeDebugArtifact(
		"compaction-event",
		{
			event: "session_before_compact",
			customInstructions: event.customInstructions,
			preparation: {
				tokensBefore: event.preparation.tokensBefore,
				firstKeptEntryId: event.preparation.firstKeptEntryId,
				previousSummaryPresent: Boolean(event.preparation.previousSummary),
				messagesToSummarizeCount: event.preparation.messagesToSummarize.length,
				turnPrefixMessagesCount: event.preparation.turnPrefixMessages.length,
			},
		},
		config,
		ctx,
	);

	if (event.signal.aborted) {
		return { cancel: true };
	}

	// Branch 1: Responses-family APIs use the native /responses/compact endpoint.
	const resolution = await resolveNativeCompactionEnvironment(ctx, {
		enabled: config.enabled,
		responsesCompactApis: config.responsesCompactApis,
	});
	if (resolution.ok) {
		const responsesOutcome = await runResponsesNativeCompact(event, ctx, config, resolution.runtime);
		if (responsesOutcome.outcome === "success") {
			return { compaction: responsesOutcome.compaction };
		}
		if (responsesOutcome.outcome === "aborted") {
			return { cancel: true };
		}
		// failed: fall through to the configured-model fallback below.
	} else {
		writeDebugArtifact(
			"compaction-event",
			{
				event: "session_before_compact.responses-compact-unavailable",
				reason: resolution.reason,
				provider: resolution.provider,
				api: resolution.api,
				model: resolution.model,
				baseUrl: resolution.baseUrl,
			},
			config,
			ctx,
		);
	}

	// Branch 2: run pi's native compaction method with the configured model.
	const fallback = await runNativeFallbackCompaction({ ctx, event, config });
	if (fallback.ok) {
		if (ctx.hasUI) {
			ctx.ui.notify(
				`${EXTENSION_ID}: compacted with ${fallback.model.provider}/${fallback.model.id} (native method)`,
				"info",
			);
		}
		writeDebugArtifact(
			"compaction-event",
			{
				event: "session_before_compact.fallback-success",
				model: fallback.model,
			},
			config,
			ctx,
		);
		return { compaction: fallback.result };
	}

	if (fallback.reason === "aborted") {
		return { cancel: true };
	}

	writeDebugArtifact(
		"compaction-event",
		{
			event: "session_before_compact.fallback-skip",
			reason: fallback.reason,
			modelSpec: fallback.modelSpec,
			errorMessage: fallback.errorMessage,
		},
		config,
		ctx,
	);

	// Intentional pi-default paths: no configured model, or it matches the current one.
	if (fallback.reason !== "no-model-configured" && fallback.reason !== "same-as-current-model") {
		notifyWarning(
			ctx,
			`compaction model "${fallback.modelSpec}" unusable (${fallback.reason}${fallback.errorMessage ? `: ${fallback.errorMessage}` : ""}); using pi's default compaction`,
		);
	}

	// Branch 3: pi's default native compaction with the current model.
	return undefined;
}

async function handleBeforeProviderRequest(event: BeforeProviderRequestEvent, ctx: ExtensionContext) {
	const { config } = loadExtensionConfig();
	if (!config.enabled) {
		return undefined;
	}

	// Capture compact-relevant request fields (tools, reasoning, ...) for the next
	// /responses/compact call, regardless of whether this request gets rewritten.
	if (isResponsesCompatiblePayload(event.payload)) {
		rememberRequestContext(event.payload, getSessionId(ctx));
	}

	const resolution = await resolveNativeCompactionEnvironment(
		ctx,
		{
			enabled: config.enabled,
			responsesCompactApis: config.responsesCompactApis,
		},
		event.payload,
	);
	if (resolution.ok === false) {
		writeDebugArtifact(
			"provider-request",
			{
				event: "before_provider_request.skip",
				reason: resolution.reason,
				provider: resolution.provider,
				api: resolution.api,
				model: resolution.model,
				baseUrl: resolution.baseUrl,
				currentModel: getCurrentModelDebugInfo(ctx),
				payload: event.payload,
			},
			config,
			ctx,
		);
		return undefined;
	}

	const runtime = resolution.runtime;
	const branchEntries = ctx.sessionManager.getBranch();
	const latestNativeCompaction = resolveLatestNativeCompactionEntry(branchEntries, {
		provider: runtime.provider,
		api: runtime.api,
		model: runtime.model,
		baseUrl: runtime.baseUrl,
	});
	if (!latestNativeCompaction.ok) {
		writeDebugArtifact(
			"provider-request",
			{
				event: "before_provider_request.no-native-compaction",
				reason: latestNativeCompaction.reason,
				provider: runtime.provider,
				api: runtime.api,
				model: runtime.model,
				baseUrl: runtime.baseUrl,
				branchEntries: branchEntries.length,
				latestCompactionIndex: latestNativeCompaction.latestCompactionIndex,
				latestCompactionIdentity: getCompactionIdentityDebugInfo(latestNativeCompaction.latestCompaction),
				payload: runtime.payload,
			},
			config,
			ctx,
		);
		return undefined;
	}

	const latestNativeCompactionEntry = latestNativeCompaction.entry;
	const rewrite = rewriteResponsesPayloadWithNativeReplay({
		model: runtime.currentModel,
		payload: runtime.payload,
		branchEntries,
		compactionEntry: latestNativeCompactionEntry,
	});
	if (!rewrite.ok) {
		writeDebugArtifact(
			"provider-request",
			{
				event: "before_provider_request.rewrite-failed",
				reason: rewrite.reason,
				provider: runtime.provider,
				api: runtime.api,
				model: runtime.model,
				baseUrl: runtime.baseUrl,
				compactionEntryId: latestNativeCompactionEntry.id,
				parity: rewrite.parity,
				payload: runtime.payload,
			},
			config,
			ctx,
		);
		return undefined;
	}

	writeDebugArtifact(
		"provider-request",
		{
			event: "before_provider_request.native-rewrite",
			provider: runtime.provider,
			api: runtime.api,
			model: runtime.model,
			baseUrl: runtime.baseUrl,
			compactionEntryId: latestNativeCompactionEntry.id,
			boundaryIndex: rewrite.segments.boundaryIndex,
			firstKeptEntryIndex: rewrite.segments.firstKeptEntryIndex,
			originalInputItems: runtime.payload.input.length,
			rewrittenInputItems: rewrite.rewrittenPayload.input.length,
			freshPreambleItems: rewrite.segments.freshPreamble.length,
			trailingPreambleItems: rewrite.segments.trailingPreamble.length,
			compactionSummaryItems: rewrite.segments.compactionSummary.length,
			preCompactionKeptItems: rewrite.segments.preCompactionKeptWindow.input.length,
			compactedItems: rewrite.segments.compactedWindow.length,
			postCompactionTailItems: rewrite.segments.postCompactionTail.input.length,
			payload: rewrite.rewrittenPayload,
			originalPayload: runtime.payload,
		},
		config,
		ctx,
	);

	return rewrite.rewrittenPayload;
}

export default function (pi: ExtensionAPI) {
	pi.on("session_start", (_event, ctx) => {
		const { config, source, warnings } = loadExtensionConfig();
		if (!config.enabled) return;

		if (warnings.length > 0 && ctx.hasUI && config.debug) {
			ctx.ui.notify(`${EXTENSION_ID}: ${warnings[0]}`, "warning");
		}

		const artifactPath = writeDebugArtifact(
			"lifecycle",
			{
				event: "session_start",
				config,
				configSource: source,
				warnings,
			},
			config,
			ctx,
		);

		if (ctx.hasUI && (config.notifyOnLoad || config.debug)) {
			ctx.ui.notify(
				artifactPath
					? `${EXTENSION_ID} loaded • debug artifacts → ${artifactPath}`
					: `${EXTENSION_ID} loaded`,
				"info",
			);
		}
	});

	pi.on("session_before_compact", handleSessionBeforeCompact);
	pi.on("before_provider_request", handleBeforeProviderRequest);
}
