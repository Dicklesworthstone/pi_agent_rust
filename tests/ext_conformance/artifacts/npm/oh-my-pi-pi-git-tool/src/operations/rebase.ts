import { GitErrorCode } from "../errors";
import { renderRebase } from "../render";
import type { RebaseParams, RebaseResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

async function getConflicts(cwd?: string): Promise<string[]> {
	const result = await git(["diff", "--name-only", "--diff-filter=U"], { cwd });
	if (result.error) return [];
	return result.stdout.split("\n").filter(Boolean);
}

export async function rebase(params: RebaseParams, cwd?: string): Promise<ToolResult<RebaseResult> | ToolError> {
	const args = ["rebase"];
	if (params.abort) args.push("--abort");
	if (params.continue) args.push("--continue");
	if (params.skip) args.push("--skip");
	if (!params.abort && !params.continue && !params.skip) {
		if (params.onto) args.push("--onto", params.onto);
		if (params.upstream) args.push(params.upstream);
	}

	const result = await git(args, { cwd });
	if (result.error) {
		if (result.error.code === GitErrorCode.REBASE_CONFLICT || result.error.code === GitErrorCode.MERGE_CONFLICT) {
			const conflicts = await getConflicts(cwd);
			const data: RebaseResult = { status: "conflict", conflicts };
			return { data, _rendered: renderRebase(data) };
		}
		return { error: result.error.message, code: result.error.code };
	}

	const output = `${result.stdout}\n${result.stderr}`.toLowerCase();
	const status: RebaseResult["status"] = output.includes("up to date") ? "up-to-date" : "success";
	const data: RebaseResult = { status };
	return { data, _rendered: renderRebase(data) };
}
