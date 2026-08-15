import type { ThinkingLevel } from "@earendil-works/pi-agent-core";
import type { CompactionEntry, CompactionResult, ExtensionContext } from "@earendil-works/pi-coding-agent";

export const EXTENSION_ID = "pi-better-compaction";
export const DEFAULT_ARTIFACT_ROOT = "~/.pi/agent/artifacts/pi-better-compaction";
export const REDACTED_VALUE = "[REDACTED]";
/**
 * APIs the extension knows how to build a `/responses/compact` URL for.
 * `responsesCompactApis` in config.json may only narrow this set.
 */
export const RESPONSES_COMPACT_CAPABLE_APIS = ["openai-responses", "openai-codex-responses"] as const;
export const NATIVE_COMPACTION_STRATEGY = "openai-native-compact-v1";
/** Used as CompactionEntry.summary only when no summary text could be extracted from the compact response. */
export const NATIVE_COMPACTION_FALLBACK_SUMMARY = "[OpenAI native compaction checkpoint]";

export const THINKING_LEVELS: readonly ThinkingLevel[] = [
	"off",
	"minimal",
	"low",
	"medium",
	"high",
	"xhigh",
	"max",
];

export type DebugArtifactKind =
	| "provider-request"
	| "compact-response"
	| "compaction-event"
	| "lifecycle";

export type ExtensionConfig = {
	enabled: boolean;
	/**
	 * Allow a Responses session whose latest compaction was not created by this extension
	 * to restart native compaction from Pi's current serialized session context.
	 */
	allowCompactionContinuityBreak: boolean;
	/**
	 * "provider/model-id" used for native-method fallback compaction (non-Responses APIs,
	 * or when the compact endpoint fails). Unset = current model via pi's default path.
	 */
	compactionModel?: string;
	/** Thinking level passed to pi's native compact() when the fallback model runs. */
	compactionThinkingLevel: ThinkingLevel;
	/** Subset of RESPONSES_COMPACT_CAPABLE_APIS that should use the compact endpoint. */
	responsesCompactApis: string[];
	notifyOnLoad: boolean;
	debug: boolean;
	logProviderPayloads: boolean;
	logCompactResponses: boolean;
	redactSensitiveData: boolean;
	artifactRoot: string;
};

export type LoadedExtensionConfig = {
	config: ExtensionConfig;
	/** Path of the config file that was applied, if it existed and parsed. */
	source?: string;
	warnings: string[];
};

export type ArtifactPaths = {
	rootDir: string;
	sessionDir: string;
	providerRequestsDir: string;
	compactResponsesDir: string;
	compactionDir: string;
	lifecycleDir: string;
};

export type ArtifactSessionInfo = {
	cwd: string;
	sessionId?: string;
	sessionFile?: string;
	sessionDir?: string;
};

export type ArtifactContext = ArtifactSessionInfo | Pick<ExtensionContext, "cwd" | "sessionManager">;

export type DebugArtifactEnvelope = {
	extension: string;
	kind: DebugArtifactKind;
	timestamp: string;
	cwd: string;
	sessionId?: string;
	sessionFile?: string;
	sessionDir?: string;
	redaction: {
		enabled: boolean;
	};
	data: unknown;
};

export type RedactOptions = {
	placeholder?: string;
};

export type NativeCompactionStrategy = typeof NATIVE_COMPACTION_STRATEGY;

export type NativeCompactionRequestMeta = {
	tokensBefore?: number;
	previousSummaryPresent?: boolean;
};

export type NativeCompactionIdentity = {
	provider: string;
	api: string;
	model: string;
	baseUrl: string;
};

export type NativeCompactionDetails = NativeCompactionIdentity & {
	strategy: NativeCompactionStrategy;
	compactedWindow: unknown[];
	compactResponseId?: string;
	createdAt: string;
	requestMeta?: NativeCompactionRequestMeta;
};

export type NativeCompactionEntry = CompactionEntry<NativeCompactionDetails>;

export type CreateNativeCompactionDetailsInput = NativeCompactionIdentity & {
	compactedWindow: unknown[];
	compactResponseId?: string;
	createdAt?: string;
	requestMeta?: NativeCompactionRequestMeta;
};

export type CreateNativeCompactionResultInput = {
	firstKeptEntryId: string;
	tokensBefore: number;
	details: NativeCompactionDetails;
	/**
	 * Summary text extracted from the compact response. Stored as the entry summary so
	 * pi's default replay still has real context after switching to an unsupported model.
	 */
	summary?: string;
};

function isRecord(value: unknown): value is Record<string, unknown> {
	return !!value && typeof value === "object" && !Array.isArray(value);
}

function isNonEmptyString(value: unknown): value is string {
	return typeof value === "string" && value.trim().length > 0;
}

function isFiniteNonNegativeNumber(value: unknown): value is number {
	return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function normalizeString(value: string): string {
	return value.trim();
}

function isStructuredValue(value: unknown): boolean {
	if (
		value === null ||
		typeof value === "string" ||
		typeof value === "number" ||
		typeof value === "boolean"
	) {
		return true;
	}

	if (Array.isArray(value)) {
		return value.every(isStructuredValue);
	}

	if (isRecord(value)) {
		return Object.values(value).every(isStructuredValue);
	}

	return false;
}

function cloneStructuredValue(value: unknown): unknown {
	if (
		value === null ||
		typeof value === "string" ||
		typeof value === "number" ||
		typeof value === "boolean"
	) {
		return value;
	}

	if (Array.isArray(value)) {
		return value.map(cloneStructuredValue);
	}

	if (isRecord(value)) {
		const clone: Record<string, unknown> = {};
		for (const [key, nested] of Object.entries(value)) {
			clone[key] = cloneStructuredValue(nested);
		}
		return clone;
	}

	throw new Error(`Unsupported structured value: ${typeof value}`);
}

function isCompactedWindowItem(value: unknown): value is Record<string, unknown> {
	return isRecord(value) && Object.values(value).every(isStructuredValue);
}

export function isNativeCompactionRequestMeta(value: unknown): value is NativeCompactionRequestMeta {
	if (!isRecord(value)) {
		return false;
	}

	const { tokensBefore, previousSummaryPresent } = value;
	if (tokensBefore !== undefined && !isFiniteNonNegativeNumber(tokensBefore)) {
		return false;
	}

	if (previousSummaryPresent !== undefined && typeof previousSummaryPresent !== "boolean") {
		return false;
	}

	return true;
}

export function isNativeCompactionIdentity(value: unknown): value is NativeCompactionIdentity {
	if (!isRecord(value)) {
		return false;
	}

	return (
		isNonEmptyString(value.provider) &&
		isNonEmptyString(value.api) &&
		isNonEmptyString(value.model) &&
		isNonEmptyString(value.baseUrl)
	);
}

export function isNativeCompactionDetails(value: unknown): value is NativeCompactionDetails {
	if (!isRecord(value)) {
		return false;
	}

	return (
		value.strategy === NATIVE_COMPACTION_STRATEGY &&
		isNativeCompactionIdentity(value) &&
		Array.isArray(value.compactedWindow) &&
		value.compactedWindow.every(isCompactedWindowItem) &&
		isNonEmptyString(value.createdAt) &&
		(value.compactResponseId === undefined || isNonEmptyString(value.compactResponseId)) &&
		(value.requestMeta === undefined || isNativeCompactionRequestMeta(value.requestMeta))
	);
}

export function isNativeCompactionEntry(value: unknown): value is NativeCompactionEntry {
	return isRecord(value) && value.type === "compaction" && isNativeCompactionDetails(value.details);
}

export function createNativeCompactionDetails(input: CreateNativeCompactionDetailsInput): NativeCompactionDetails {
	return {
		strategy: NATIVE_COMPACTION_STRATEGY,
		provider: normalizeString(input.provider),
		api: normalizeString(input.api),
		model: normalizeString(input.model),
		baseUrl: normalizeString(input.baseUrl),
		compactedWindow: input.compactedWindow.map((item) => cloneStructuredValue(item)),
		compactResponseId: isNonEmptyString(input.compactResponseId) ? normalizeString(input.compactResponseId) : undefined,
		createdAt: isNonEmptyString(input.createdAt) ? normalizeString(input.createdAt) : new Date().toISOString(),
		requestMeta: input.requestMeta
			? {
				...(input.requestMeta.tokensBefore !== undefined ? { tokensBefore: input.requestMeta.tokensBefore } : {}),
				...(input.requestMeta.previousSummaryPresent !== undefined
					? { previousSummaryPresent: input.requestMeta.previousSummaryPresent }
					: {}),
			}
			: undefined,
	};
}

export function createNativeCompactionResult(
	input: CreateNativeCompactionResultInput,
): CompactionResult<NativeCompactionDetails> {
	const summary = input.summary?.trim();
	return {
		summary: summary && summary.length > 0 ? summary : NATIVE_COMPACTION_FALLBACK_SUMMARY,
		firstKeptEntryId: input.firstKeptEntryId,
		tokensBefore: input.tokensBefore,
		details: input.details,
	};
}

export const DEFAULT_EXTENSION_CONFIG: ExtensionConfig = {
	enabled: true,
	allowCompactionContinuityBreak: false,
	compactionModel: undefined,
	compactionThinkingLevel: "off",
	responsesCompactApis: [...RESPONSES_COMPACT_CAPABLE_APIS],
	notifyOnLoad: false,
	debug: false,
	logProviderPayloads: false,
	logCompactResponses: false,
	redactSensitiveData: true,
	artifactRoot: DEFAULT_ARTIFACT_ROOT,
};
