import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Api, Model } from "@earendil-works/pi-ai";
import type {
	BranchSummaryEntry,
	CustomMessageEntry,
	SessionEntry,
	SessionMessageEntry,
} from "@earendil-works/pi-coding-agent";
import type { ResponsesCompatibleRequestPayload } from "./runtime";
import type { NativeCompactionEntry } from "./types";
import {
	compareResponsesInputParity,
	serializeMessagesToResponsesInput,
	type ResponsesInputContentItem,
	type ResponsesInputItem,
	type ResponsesInputMessageItem,
} from "./serializer";

export type FreshAuthoritativePreamble = {
	instructions?: string;
	leadingInput: ResponsesInputMessageItem[];
	trailingInput: ResponsesInputMessageItem[];
};

export type SerializedReplaySlice = {
	entries: SessionEntry[];
	messages: AgentMessage[];
	input: ResponsesInputItem[];
};

export type NativeReplaySegments = {
	boundaryIndex: number;
	firstKeptEntryIndex: number;
	instructions?: string;
	freshPreamble: ResponsesInputMessageItem[];
	trailingPreamble: ResponsesInputMessageItem[];
	compactionSummary: ResponsesInputItem[];
	preCompactionKeptWindow: SerializedReplaySlice;
	compactedWindow: unknown[];
	postCompactionTail: SerializedReplaySlice;
	originalPiReplayInput: ResponsesInputItem[];
	replayInput: unknown[];
};

export type NativeReplayPayloadRewrite = {
	ok: true;
	segments: NativeReplaySegments;
	rewrittenPayload: ResponsesCompatibleRequestPayload;
};

export type NativeReplayPayloadRewriteFailureReason =
	| "compaction-boundary-not-found"
	| "first-kept-entry-not-found"
	| "unsupported-instructions"
	| "invalid-compacted-window"
	| "unexpected-compaction-after-boundary"
	| "expected-pi-replay-mismatch";

export type NativeReplayPayloadRewriteFailure = {
	ok: false;
	reason: NativeReplayPayloadRewriteFailureReason;
	parity?: {
		actual: string[];
		expected: string[];
		mismatches: string[];
	};
};

export type NativeReplayPayloadRewriteResult =
	| NativeReplayPayloadRewrite
	| NativeReplayPayloadRewriteFailure;

function isRecord(value: unknown): value is Record<string, unknown> {
	return !!value && typeof value === "object" && !Array.isArray(value);
}

function isResponsesInputContentItem(value: unknown): value is ResponsesInputContentItem {
	if (!isRecord(value) || typeof value.type !== "string") {
		return false;
	}

	if (value.type === "input_text") {
		return typeof value.text === "string";
	}

	if (value.type === "input_image") {
		return value.detail === "auto" && typeof value.image_url === "string";
	}

	return false;
}

function isResponsesInputMessageRole(value: unknown): value is ResponsesInputMessageItem["role"] {
	return value === "user" || value === "developer" || value === "system";
}

function isPreambleRole(value: ResponsesInputMessageItem["role"]): value is "developer" | "system" {
	return value === "developer" || value === "system";
}

function isResponsesInputMessageItem(value: unknown): value is ResponsesInputMessageItem {
	if (!isRecord(value) || !isResponsesInputMessageRole(value.role)) {
		return false;
	}

	const { content } = value;
	return typeof content === "string" || (Array.isArray(content) && content.every(isResponsesInputContentItem));
}

function cloneResponsesInputContentItem(item: ResponsesInputContentItem): ResponsesInputContentItem {
	return item.type === "input_text"
		? {
			type: "input_text",
			text: item.text,
		}
		: {
			type: "input_image",
			detail: "auto",
			image_url: item.image_url,
		};
}

function cloneResponsesInputMessageItem(item: ResponsesInputMessageItem): ResponsesInputMessageItem {
	return {
		role: item.role,
		content: typeof item.content === "string" ? item.content : item.content.map(cloneResponsesInputContentItem),
	};
}

function cloneStructuredValue(value: unknown): unknown {
	if (
		value === undefined ||
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

function cloneOpaqueCompactedWindow(compactedWindow: readonly unknown[]): unknown[] | undefined {
	const cloned: unknown[] = [];

	for (const item of compactedWindow) {
		if (!isRecord(item)) {
			return undefined;
		}

		try {
			cloned.push(cloneStructuredValue(item));
		} catch {
			return undefined;
		}
	}

	return cloned;
}

function cloneResponsesInputSlice(items: readonly unknown[]): ResponsesInputItem[] | undefined {
	const cloned: ResponsesInputItem[] = [];

	for (const item of items) {
		try {
			cloned.push(cloneStructuredValue(item) as ResponsesInputItem);
		} catch {
			return undefined;
		}
	}

	return cloned;
}

function areEquivalentValues(left: unknown, right: unknown): boolean {
	if (Object.is(left, right)) {
		return true;
	}

	if (Array.isArray(left) || Array.isArray(right)) {
		if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
			return false;
		}

		for (let index = 0; index < left.length; index++) {
			if (!areEquivalentValues(left[index], right[index])) {
				return false;
			}
		}

		return true;
	}

	if (isRecord(left) || isRecord(right)) {
		if (!isRecord(left) || !isRecord(right)) {
			return false;
		}

		const leftKeys = Object.keys(left).sort();
		const rightKeys = Object.keys(right).sort();
		if (!areEquivalentValues(leftKeys, rightKeys)) {
			return false;
		}

		for (const key of leftKeys) {
			if (!areEquivalentValues(left[key], right[key])) {
				return false;
			}
		}

		return true;
	}

	return false;
}

function toBranchSummaryMessage(entry: BranchSummaryEntry): AgentMessage {
	return {
		role: "branchSummary",
		summary: entry.summary,
		fromId: entry.fromId,
		timestamp: new Date(entry.timestamp).getTime(),
	} as AgentMessage;
}

function toCustomMessage(entry: CustomMessageEntry): AgentMessage {
	return {
		role: "custom",
		customType: entry.customType,
		content: entry.content,
		display: entry.display,
		details: entry.details,
		timestamp: new Date(entry.timestamp).getTime(),
	} as AgentMessage;
}

function toSessionMessage(entry: SessionMessageEntry): AgentMessage {
	return entry.message;
}

function toReplayAgentMessage(entry: SessionEntry): AgentMessage | undefined {
	if (entry.type === "message") {
		return toSessionMessage(entry);
	}

	if (entry.type === "custom_message") {
		return toCustomMessage(entry);
	}

	if (entry.type === "branch_summary") {
		return toBranchSummaryMessage(entry);
	}

	return undefined;
}

function isPromptEnvelopeItem(item: unknown): item is ResponsesInputMessageItem {
	return isResponsesInputMessageItem(item) && isPreambleRole(item.role);
}

export function extractFreshAuthoritativePreamble(
	payload: ResponsesCompatibleRequestPayload,
): FreshAuthoritativePreamble | undefined {
	if (payload.instructions !== undefined && typeof payload.instructions !== "string") {
		return undefined;
	}

	// Developer/system items in Pi's Responses payload are prompt-level instructions,
	// not transcript entries from session history. Preserve them in the same leading
	// or trailing position that Pi authored so provider-added suffix prompts like
	// GPT-5's trailing developer "# Juice: 0 !important" survive replay unchanged.
	let leadingBoundary = 0;
	while (leadingBoundary < payload.input.length && isPromptEnvelopeItem(payload.input[leadingBoundary])) {
		leadingBoundary += 1;
	}

	let trailingBoundary = payload.input.length;
	while (trailingBoundary > leadingBoundary && isPromptEnvelopeItem(payload.input[trailingBoundary - 1])) {
		trailingBoundary -= 1;
	}

	for (let index = leadingBoundary; index < trailingBoundary; index++) {
		if (isPromptEnvelopeItem(payload.input[index])) {
			return undefined;
		}
	}

	return {
		...(typeof payload.instructions === "string" ? { instructions: payload.instructions } : {}),
		leadingInput: payload.input.slice(0, leadingBoundary).map((item) => cloneResponsesInputMessageItem(item as ResponsesInputMessageItem)),
		trailingInput: payload.input
			.slice(trailingBoundary)
			.map((item) => cloneResponsesInputMessageItem(item as ResponsesInputMessageItem)),
	};
}

function collectReplayMessages(entries: readonly SessionEntry[]): AgentMessage[] {
	const messages: AgentMessage[] = [];

	for (const entry of entries) {
		const message = toReplayAgentMessage(entry);
		if (message) {
			messages.push(message);
		}
	}

	return messages;
}

function createCompactionSummaryAgentMessage(entry: NativeCompactionEntry): AgentMessage {
	return {
		role: "compactionSummary",
		summary: entry.summary,
		tokensBefore: entry.tokensBefore,
		timestamp: new Date(entry.timestamp).getTime(),
	} as AgentMessage;
}

function createReplaySlice(
	entries: readonly SessionEntry[],
	messages: readonly AgentMessage[],
	input: readonly ResponsesInputItem[],
): SerializedReplaySlice {
	return {
		entries: [...entries],
		messages: [...messages],
		input: [...input],
	};
}

function findEntryIndexByIdBeforeBoundary(
	entries: readonly SessionEntry[],
	entryId: string,
	boundaryIndex: number,
): number | undefined {
	const index = entries.findIndex((entry, candidateIndex) => candidateIndex < boundaryIndex && entry.id === entryId);
	return index >= 0 ? index : undefined;
}

export function findCompactionBoundaryIndex(
	entries: readonly SessionEntry[],
	compactionEntryId: string,
): number | undefined {
	const boundaryIndex = entries.findIndex((entry) => entry.id === compactionEntryId);
	return boundaryIndex >= 0 ? boundaryIndex : undefined;
}

export function findEntriesStrictlyAfterCompactionBoundary(
	entries: readonly SessionEntry[],
	compactionEntryId: string,
): SessionEntry[] | undefined {
	const boundaryIndex = findCompactionBoundaryIndex(entries, compactionEntryId);
	if (boundaryIndex === undefined) {
		return undefined;
	}

	return entries.slice(boundaryIndex + 1);
}

export function collectLiveTailMessages(entries: readonly SessionEntry[]): AgentMessage[] {
	return collectReplayMessages(entries);
}

export function serializeLiveTailToResponsesInput<TApi extends Api>(args: {
	model: Model<TApi>;
	entries: readonly SessionEntry[];
}): ResponsesInputItem[] {
	return serializeMessagesToResponsesInput(args.model, collectReplayMessages(args.entries));
}

function buildNativeReplaySegmentsInternal<TApi extends Api>(args: {
	model: Model<TApi>;
	payload: ResponsesCompatibleRequestPayload;
	branchEntries: readonly SessionEntry[];
	compactionEntry: NativeCompactionEntry;
}): NativeReplayPayloadRewriteResult {
	const boundaryIndex = findCompactionBoundaryIndex(args.branchEntries, args.compactionEntry.id);
	if (boundaryIndex === undefined) {
		return {
			ok: false,
			reason: "compaction-boundary-not-found",
		};
	}

	const firstKeptEntryIndex = findEntryIndexByIdBeforeBoundary(
		args.branchEntries,
		args.compactionEntry.firstKeptEntryId,
		boundaryIndex,
	);
	if (firstKeptEntryIndex === undefined) {
		return {
			ok: false,
			reason: "first-kept-entry-not-found",
		};
	}

	const freshPreamble = extractFreshAuthoritativePreamble(args.payload);
	if (!freshPreamble) {
		return {
			ok: false,
			reason: "unsupported-instructions",
		};
	}

	const newerCompactionEntry = args.branchEntries
		.slice(boundaryIndex + 1)
		.some((entry) => entry.type === "compaction");
	if (newerCompactionEntry) {
		return {
			ok: false,
			reason: "unexpected-compaction-after-boundary",
		};
	}

	const compactedWindow = cloneOpaqueCompactedWindow(args.compactionEntry.details.compactedWindow);
	if (!compactedWindow) {
		return {
			ok: false,
			reason: "invalid-compacted-window",
		};
	}

	const preCompactionEntries = args.branchEntries.slice(firstKeptEntryIndex, boundaryIndex);
	const postCompactionEntries = args.branchEntries.slice(boundaryIndex + 1);
	const preCompactionKeptMessages = collectReplayMessages(preCompactionEntries);
	const postCompactionTailMessages = collectReplayMessages(postCompactionEntries);
	const compactionSummaryMessage = createCompactionSummaryAgentMessage(args.compactionEntry);
	const serializedPiHistoryInput = serializeMessagesToResponsesInput(args.model, [
		compactionSummaryMessage,
		...preCompactionKeptMessages,
		...postCompactionTailMessages,
	]);
	const originalPiReplayInput: ResponsesInputItem[] = [
		...freshPreamble.leadingInput,
		...serializedPiHistoryInput,
		...freshPreamble.trailingInput,
	];

	if (!areEquivalentValues(args.payload.input, originalPiReplayInput)) {
		const parity = compareResponsesInputParity(args.payload.input, originalPiReplayInput);
		return {
			ok: false,
			reason: "expected-pi-replay-mismatch",
			parity: {
				actual: parity.actual,
				expected: parity.expected,
				mismatches: parity.mismatches,
			},
		};
	}

	const freshPreambleCount = freshPreamble.leadingInput.length;
	const trailingPreambleCount = freshPreamble.trailingInput.length;
	const compactionSummaryCount = serializeMessagesToResponsesInput(args.model, [compactionSummaryMessage]).length;
	const preCompactionKeptCount = serializeMessagesToResponsesInput(args.model, preCompactionKeptMessages).length;
	const tailStartIndex = freshPreambleCount + compactionSummaryCount + preCompactionKeptCount;
	const tailEndIndex = args.payload.input.length - trailingPreambleCount;
	const actualCompactionSummary = cloneResponsesInputSlice(
		args.payload.input.slice(freshPreambleCount, freshPreambleCount + compactionSummaryCount),
	);
	const actualPreCompactionKeptWindow = cloneResponsesInputSlice(
		args.payload.input.slice(
			freshPreambleCount + compactionSummaryCount,
			freshPreambleCount + compactionSummaryCount + preCompactionKeptCount,
		),
	);
	const actualPostCompactionTail = cloneResponsesInputSlice(args.payload.input.slice(tailStartIndex, tailEndIndex));
	if (!actualCompactionSummary || !actualPreCompactionKeptWindow || !actualPostCompactionTail) {
		return {
			ok: false,
			reason: "expected-pi-replay-mismatch",
		};
	}

	const preCompactionKeptWindow = createReplaySlice(
		preCompactionEntries,
		preCompactionKeptMessages,
		actualPreCompactionKeptWindow,
	);
	const postCompactionTail = createReplaySlice(
		postCompactionEntries,
		postCompactionTailMessages,
		actualPostCompactionTail,
	);

	return {
		ok: true,
		segments: {
			boundaryIndex,
			firstKeptEntryIndex,
			instructions: freshPreamble.instructions,
			freshPreamble: freshPreamble.leadingInput,
			trailingPreamble: freshPreamble.trailingInput,
			compactionSummary: actualCompactionSummary,
			preCompactionKeptWindow,
			compactedWindow,
			postCompactionTail,
			originalPiReplayInput,
			replayInput: [
				...freshPreamble.leadingInput,
				...compactedWindow,
				...actualPostCompactionTail,
				...freshPreamble.trailingInput,
			],
		},
		rewrittenPayload: {
			...args.payload,
			...(freshPreamble.instructions !== undefined ? { instructions: freshPreamble.instructions } : {}),
			input: [
				...freshPreamble.leadingInput,
				...compactedWindow,
				...actualPostCompactionTail,
				...freshPreamble.trailingInput,
			],
		},
	};
}

export function buildNativeReplaySegments<TApi extends Api>(args: {
	model: Model<TApi>;
	payload: ResponsesCompatibleRequestPayload;
	branchEntries: readonly SessionEntry[];
	compactionEntry: NativeCompactionEntry;
}): NativeReplayPayloadRewriteResult {
	return buildNativeReplaySegmentsInternal(args);
}

export function rewriteResponsesPayloadWithNativeReplay<TApi extends Api>(args: {
	model: Model<TApi>;
	payload: ResponsesCompatibleRequestPayload;
	branchEntries: readonly SessionEntry[];
	compactionEntry: NativeCompactionEntry;
}): NativeReplayPayloadRewriteResult {
	return buildNativeReplaySegmentsInternal(args);
}
