import type { CompactionEntry, SessionEntry } from "@earendil-works/pi-coding-agent";
import {
	isNativeCompactionDetails,
	isNativeCompactionEntry,
	type NativeCompactionDetails,
	type NativeCompactionEntry,
	type NativeCompactionIdentity,
} from "./types";

export type NativeCompactionEntryMatch = Partial<NativeCompactionIdentity>;

export type LatestNativeCompactionResolutionFailureReason =
	| "no-compaction"
	| "latest-compaction-not-native"
	| "latest-native-compaction-mismatch";

export type LatestNativeCompactionResolution =
	| {
			ok: true;
			entry: NativeCompactionEntry;
			index: number;
			latestCompactionIndex: number;
	  }
	| {
			ok: false;
			reason: LatestNativeCompactionResolutionFailureReason;
			latestCompactionIndex?: number;
			latestCompaction?: CompactionEntry;
	  };

function entryMatches(entry: NativeCompactionEntry, match: NativeCompactionEntryMatch): boolean {
	const details = entry.details;
	if (!details) {
		return false;
	}

	return (
		(match.provider === undefined || details.provider === match.provider) &&
		(match.api === undefined || details.api === match.api) &&
		(match.model === undefined || details.model === match.model) &&
		(match.baseUrl === undefined || details.baseUrl === match.baseUrl)
	);
}

export function getNativeCompactionDetails(
	entry: CompactionEntry | SessionEntry | undefined,
): NativeCompactionDetails | undefined {
	if (!entry || entry.type !== "compaction") {
		return undefined;
	}

	return isNativeCompactionDetails(entry.details) ? entry.details : undefined;
}

export function isPersistedNativeCompactionEntry(
	entry: CompactionEntry | SessionEntry | undefined,
): entry is NativeCompactionEntry {
	return isNativeCompactionEntry(entry);
}

export function findLatestCompactionEntryIndex(entries: readonly SessionEntry[]): number | undefined {
	for (let index = entries.length - 1; index >= 0; index--) {
		if (entries[index]?.type === "compaction") {
			return index;
		}
	}

	return undefined;
}

export function findLatestCompactionEntry(entries: readonly SessionEntry[]): CompactionEntry | undefined {
	const index = findLatestCompactionEntryIndex(entries);
	return index === undefined ? undefined : (entries[index] as CompactionEntry);
}

export function findLatestNativeCompactionEntryIndex(
	entries: readonly SessionEntry[],
	match: NativeCompactionEntryMatch = {},
): number | undefined {
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (!isPersistedNativeCompactionEntry(entry)) {
			continue;
		}

		if (!entryMatches(entry, match)) {
			continue;
		}

		return index;
	}

	return undefined;
}

export function findLatestNativeCompactionEntry(
	entries: readonly SessionEntry[],
	match: NativeCompactionEntryMatch = {},
): NativeCompactionEntry | undefined {
	const index = findLatestNativeCompactionEntryIndex(entries, match);
	return index === undefined ? undefined : (entries[index] as NativeCompactionEntry);
}

export function findLatestNativeCompactionDetails(
	entries: readonly SessionEntry[],
	match: NativeCompactionEntryMatch = {},
): NativeCompactionDetails | undefined {
	return findLatestNativeCompactionEntry(entries, match)?.details;
}

export function resolveLatestNativeCompactionEntry(
	entries: readonly SessionEntry[],
	match: NativeCompactionEntryMatch = {},
): LatestNativeCompactionResolution {
	const latestCompactionIndex = findLatestCompactionEntryIndex(entries);
	if (latestCompactionIndex === undefined) {
		return {
			ok: false,
			reason: "no-compaction",
		};
	}

	const latestCompaction = entries[latestCompactionIndex];
	if (!latestCompaction || latestCompaction.type !== "compaction" || !isPersistedNativeCompactionEntry(latestCompaction)) {
		return {
			ok: false,
			reason: "latest-compaction-not-native",
			latestCompactionIndex,
			latestCompaction:
				latestCompaction && latestCompaction.type === "compaction"
					? (latestCompaction as CompactionEntry)
					: undefined,
		};
	}

	if (!entryMatches(latestCompaction, match)) {
		return {
			ok: false,
			reason: "latest-native-compaction-mismatch",
			latestCompactionIndex,
			latestCompaction,
		};
	}

	return {
		ok: true,
		entry: latestCompaction,
		index: latestCompactionIndex,
		latestCompactionIndex,
	};
}
