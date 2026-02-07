import { GitErrorCode } from "../errors";
import { renderCherryPick } from "../render";
import type { CherryPickParams, CherryPickResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

async function getConflicts(cwd?: string): Promise<string[]> {
	const result = await git(["diff", "--name-only", "--diff-filter=U"], { cwd });
	if (result.error) return [];
	return result.stdout.split("\n").filter(Boolean);
}

export async function cherryPick(
	params: CherryPickParams,
	cwd?: string,
): Promise<ToolResult<CherryPickResult> | ToolError> {
	const args = ["cherry-pick"];
	if (params.abort) args.push("--abort");
	if (params.continue) args.push("--continue");
	if (params.no_commit) args.push("--no-commit");
	if (!params.abort && !params.continue) {
		if (!params.commits || params.commits.length === 0) {
			return { error: "Commits are required for cherry-pick" };
		}
		args.push(...params.commits);
	}

	const result = await git(args, { cwd });
	if (result.error) {
		if (result.error.code === GitErrorCode.MERGE_CONFLICT) {
			const conflicts = await getConflicts(cwd);
			const data: CherryPickResult = { status: "conflict", conflicts };
			return { data, _rendered: renderCherryPick(data) };
		}
		return { error: result.error.message, code: result.error.code };
	}

	const data: CherryPickResult = { status: "success", appliedCommits: params.commits };
	return { data, _rendered: renderCherryPick(data) };
}
