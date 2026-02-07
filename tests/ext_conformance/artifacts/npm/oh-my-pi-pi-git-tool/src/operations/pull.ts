import { GitErrorCode } from "../errors";
import { renderPull } from "../render";
import type { PullParams, PullResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

async function getConflicts(cwd?: string): Promise<string[]> {
	const result = await git(["diff", "--name-only", "--diff-filter=U"], { cwd });
	if (result.error) return [];
	return result.stdout.split("\n").filter(Boolean);
}

async function countPulledCommits(cwd?: string): Promise<number> {
	const origResult = await git(["rev-parse", "-q", "--verify", "ORIG_HEAD"], { cwd });
	if (origResult.error) return 0;
	const orig = origResult.stdout.trim();
	const countResult = await git(["rev-list", "--count", `${orig}..HEAD`], { cwd });
	if (countResult.error) return 0;
	return Number.parseInt(countResult.stdout.trim(), 10);
}

export async function pull(params: PullParams, cwd?: string): Promise<ToolResult<PullResult> | ToolError> {
	const args = ["pull"];
	if (params.rebase) args.push("--rebase");
	if (params.ff_only) args.push("--ff-only");
	if (params.remote) args.push(params.remote);
	if (params.branch) args.push(params.branch);

	const result = await git(args, { cwd });
	if (result.error) {
		if (result.error.code === GitErrorCode.MERGE_CONFLICT || result.error.code === GitErrorCode.REBASE_CONFLICT) {
			const conflicts = await getConflicts(cwd);
			const data: PullResult = { status: "conflict", conflicts };
			return { data, _rendered: renderPull(data) };
		}
		return { error: result.error.message, code: result.error.code };
	}

	const output = `${result.stdout}\n${result.stderr}`.toLowerCase();
	if (output.includes("already up to date")) {
		const data: PullResult = { status: "up-to-date" };
		return { data, _rendered: renderPull(data) };
	}

	const commits = await countPulledCommits(cwd);
	const data: PullResult = { status: "success", commits };
	return { data, _rendered: renderPull(data) };
}
