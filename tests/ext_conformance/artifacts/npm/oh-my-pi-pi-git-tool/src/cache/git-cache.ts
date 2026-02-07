import type { BranchListResult, Commit, StatusResult } from "../types";

export interface CacheEntry<T> {
	value: T;
	timestamp: number;
	ttl: number;
	cwd: string;
}

export interface GitCache {
	branch: CacheEntry<BranchListResult> | null;
	status: CacheEntry<StatusResult> | null;
	remotes: CacheEntry<Array<{ name: string; url: string }>> | null;
	commits: Map<string, CacheEntry<Commit>>;
}

export const DEFAULT_TTL = {
	branch: 30_000,
	status: 5_000,
	remotes: 60_000,
	commits: 300_000,
};

export function createCache(): GitCache {
	return {
		branch: null,
		status: null,
		remotes: null,
		commits: new Map(),
	};
}

export function isExpired(entry: CacheEntry<unknown>): boolean {
	return Date.now() - entry.timestamp > entry.ttl;
}
