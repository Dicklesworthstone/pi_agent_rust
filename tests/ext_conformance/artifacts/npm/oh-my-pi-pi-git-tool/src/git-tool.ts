import { createCache, DEFAULT_TTL, isExpired } from "./cache/git-cache";
import { add } from "./operations/add";
import { blame } from "./operations/blame";
import { branch } from "./operations/branch";
import { checkout } from "./operations/checkout";
import { cherryPick } from "./operations/cherry-pick";
import { commit } from "./operations/commit";
import { diff } from "./operations/diff";
import { fetch } from "./operations/fetch";
import { ci } from "./operations/github/ci";
import { issue } from "./operations/github/issue";
import { pr } from "./operations/github/pr";
import { release } from "./operations/github/release";
import { log } from "./operations/log";
import { merge } from "./operations/merge";
import { pull } from "./operations/pull";
import { push } from "./operations/push";
import { rebase } from "./operations/rebase";
import { restore } from "./operations/restore";
import { show } from "./operations/show";
import { stash } from "./operations/stash";
import { status } from "./operations/status";
import { tag } from "./operations/tag";
import { renderBranchList, renderStatus } from "./render";
import { checkSafety } from "./safety/guards";
import type { BranchListResult, GitParams, Operation, StatusResult, ToolResponse, ToolResult } from "./types";
import { isTruthy } from "./utils";

const cache = createCache();

type OperationHandler = (params: GitParams, cwd?: string) => Promise<ToolResponse<unknown>>;

const operations: Record<Operation, OperationHandler> = {
	status: status as OperationHandler,
	diff: diff as OperationHandler,
	log: log as OperationHandler,
	show: show as OperationHandler,
	blame: blame as OperationHandler,
	branch: branch as OperationHandler,
	add: add as OperationHandler,
	restore: restore as OperationHandler,
	commit: commit as OperationHandler,
	checkout: checkout as OperationHandler,
	merge: merge as OperationHandler,
	rebase: rebase as OperationHandler,
	stash: stash as OperationHandler,
	"cherry-pick": cherryPick as OperationHandler,
	fetch: fetch as OperationHandler,
	pull: pull as OperationHandler,
	push: push as OperationHandler,
	tag: tag as OperationHandler,
	pr: pr as OperationHandler,
	issue: issue as OperationHandler,
	ci: ci as OperationHandler,
	release: release as OperationHandler,
};

const READ_OPERATIONS: Operation[] = ["status", "diff", "log", "show", "blame", "branch"];
const WRITE_OPERATIONS: Operation[] = [
	"add",
	"restore",
	"commit",
	"checkout",
	"merge",
	"rebase",
	"stash",
	"cherry-pick",
	"pull",
	"fetch",
];

function invalidateOnWrite(operation: Operation): void {
	if (WRITE_OPERATIONS.includes(operation)) {
		cache.status = null;
		if (["checkout", "merge", "rebase"].includes(operation)) {
			cache.branch = null;
		}
	}
}

function cacheStatus(result: ToolResult<StatusResult>, cwd: string): void {
	cache.status = { value: result.data, timestamp: Date.now(), ttl: DEFAULT_TTL.status, cwd };
}

function cacheBranch(result: ToolResult<BranchListResult>, cwd: string): void {
	cache.branch = { value: result.data, timestamp: Date.now(), ttl: DEFAULT_TTL.branch, cwd };
}

function getCachedStatus(cwd: string): ToolResult<StatusResult> | null {
	if (!cache.status) return null;
	if (isExpired(cache.status)) {
		cache.status = null;
		return null;
	}
	if (cache.status.cwd !== cwd) return null;
	return {
		data: cache.status.value,
		_rendered: renderStatus(cache.status.value),
	};
}

function getCachedBranch(cwd: string): ToolResult<BranchListResult> | null {
	if (!cache.branch) return null;
	if (isExpired(cache.branch)) {
		cache.branch = null;
		return null;
	}
	if (cache.branch.cwd !== cwd) return null;
	return {
		data: cache.branch.value,
		_rendered: renderBranchList(cache.branch.value),
	};
}

export async function gitTool(params: GitParams, cwd?: string): Promise<ToolResponse<unknown>> {
	const resolvedCwd = cwd ?? process.cwd();
	const operation = params.operation as Operation;
	const handler = operations[operation];
	if (!handler) {
		return { error: `Unknown operation: ${operation}` };
	}

	const paramRecord = params as unknown as Record<string, unknown>;
	const safety = await checkSafety(operation, paramRecord, resolvedCwd);
	if (safety.blocked) {
		const overrideValue = safety.override ? paramRecord[safety.override] : undefined;
		if (!safety.override || !isTruthy(String(overrideValue ?? ""))) {
			return { error: safety.message ?? "Operation blocked", suggestion: safety.suggestion, code: "SAFETY_BLOCK" };
		}
	}
	if (safety.confirm) {
		const overrideValue = safety.override ? paramRecord[safety.override] : undefined;
		if (!safety.override || !isTruthy(String(overrideValue ?? ""))) {
			return {
				confirm: safety.message ?? "Confirmation required",
				override: safety.override ?? "confirm",
				_rendered: safety.message,
			};
		}
	}

	if (READ_OPERATIONS.includes(operation)) {
		if (operation === "status") {
			const cached = getCachedStatus(resolvedCwd);
			const statusParams = params as { only?: string; ignored?: boolean };
			if (cached && !statusParams.only && !statusParams.ignored) return cached;
		}
		if (operation === "branch") {
			const cached = getCachedBranch(resolvedCwd);
			const branchParams = params as { action?: string; remotes?: boolean };
			if (cached && (!branchParams.action || branchParams.action === "list") && !branchParams.remotes) return cached;
		}
	}

	const result = await handler(params, resolvedCwd);

	invalidateOnWrite(operation);
	if ("data" in result && READ_OPERATIONS.includes(operation)) {
		if (operation === "status") cacheStatus(result as ToolResult<StatusResult>, resolvedCwd);
		if (operation === "branch") cacheBranch(result as ToolResult<BranchListResult>, resolvedCwd);
	}

	if ("data" in result && safety.warnings.length > 0) {
		const suffix = `\n\nWarnings:\n${safety.warnings.map((warn) => `- ${warn}`).join("\n")}`;
		result._rendered = `${result._rendered ?? ""}${suffix}`;
	}

	return result;
}
