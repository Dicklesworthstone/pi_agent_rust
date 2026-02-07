import { GitErrorCode } from "../errors";
import { renderMerge } from "../render";
import type { MergeParams, MergeResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

async function getConflicts(cwd?: string): Promise<string[]> {
	const result = await git(["diff", "--name-only", "--diff-filter=U"], { cwd });
	if (result.error) return [];
	return result.stdout.split("\n").filter(Boolean);
}

export async function merge(params: MergeParams, cwd?: string): Promise<ToolResult<MergeResult> | ToolError> {
	if (params.abort) {
		const result = await git(["merge", "--abort"], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: MergeResult = { status: "success" };
		return { data, _rendered: renderMerge(data) };
	}

	if (params.continue) {
		const result = await git(["merge", "--continue"], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const shaResult = await git(["rev-parse", "HEAD"], { cwd });
		const data: MergeResult = { status: "success", sha: shaResult.error ? undefined : shaResult.stdout.trim() };
		return { data, _rendered: renderMerge(data) };
	}

	const args = ["merge"];
	if (params.no_ff) args.push("--no-ff");
	if (params.ff_only) args.push("--ff-only");
	if (params.squash) args.push("--squash");
	if (params.message) args.push("-m", params.message);
	if (!params.ref) {
		return { error: "Merge ref is required" };
	}
	args.push(params.ref);

	const result = await git(args, { cwd });
	if (result.error) {
		if (result.error.code === GitErrorCode.MERGE_CONFLICT) {
			const conflicts = await getConflicts(cwd);
			const data: MergeResult = { status: "conflict", conflicts };
			return { data, _rendered: renderMerge(data) };
		}
		return { error: result.error.message, code: result.error.code };
	}

	const output = `${result.stdout}\n${result.stderr}`.toLowerCase();
	let status: MergeResult["status"] = "success";
	if (output.includes("already up to date")) {
		status = "up-to-date";
	} else if (output.includes("fast-forward")) {
		status = "fast-forward";
	}
	const shaResult = await git(["rev-parse", "HEAD"], { cwd });
	const data: MergeResult = { status, sha: shaResult.error ? undefined : shaResult.stdout.trim() };
	return { data, _rendered: renderMerge(data) };
}
