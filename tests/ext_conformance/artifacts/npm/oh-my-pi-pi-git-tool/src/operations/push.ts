import { renderPush } from "../render";
import type { PushParams, PushResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

async function getCurrentBranch(cwd?: string): Promise<string> {
	const result = await git(["rev-parse", "--abbrev-ref", "HEAD"], { cwd });
	if (result.error) return "";
	return result.stdout.trim();
}

async function hasRemoteRef(remote: string, branch: string, cwd?: string): Promise<boolean> {
	const result = await git(["rev-parse", "--verify", `refs/remotes/${remote}/${branch}`], { cwd });
	return !result.error;
}

async function countCommitsToPush(remote: string, branch: string, hasRemote: boolean, cwd?: string): Promise<number> {
	if (hasRemote) {
		const result = await git(["rev-list", "--count", `refs/remotes/${remote}/${branch}..${branch}`], { cwd });
		if (result.error) return 0;
		return Number.parseInt(result.stdout.trim(), 10);
	}
	const result = await git(["rev-list", "--count", branch], { cwd });
	if (result.error) return 0;
	return Number.parseInt(result.stdout.trim(), 10);
}

export async function push(params: PushParams, cwd?: string): Promise<ToolResult<PushResult> | ToolError> {
	const remote = params.remote ?? "origin";
	const branch = params.branch ?? (await getCurrentBranch(cwd));
	if (!branch) {
		return { error: "Branch not found", code: "BRANCH_NOT_FOUND" };
	}

	if (params.delete) {
		const result = await git(["push", remote, "--delete", branch], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PushResult = { remote, branch, commits: 0, newBranch: false };
		return { data, _rendered: renderPush(data) };
	}

	const hasRemote = await hasRemoteRef(remote, branch, cwd);
	const commits = await countCommitsToPush(remote, branch, hasRemote, cwd);

	const args = ["push", remote, branch];
	if (params.set_upstream) args.push("--set-upstream");
	if (params.tags) args.push("--tags");
	if (params.force_with_lease) args.push("--force-with-lease");
	if (params.force) args.push("--force");

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const data: PushResult = { remote, branch, commits, newBranch: !hasRemote };
	return { data, _rendered: renderPush(data) };
}
