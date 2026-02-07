import { renderFetch } from "../render";
import type { FetchParams, FetchResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

async function getRemoteRefs(remote: string, branch?: string, cwd?: string): Promise<Map<string, string>> {
	const refPrefix = branch ? `refs/remotes/${remote}/${branch}` : `refs/remotes/${remote}`;
	const result = await git(["for-each-ref", refPrefix, "--format=%(refname)\t%(objectname)"], { cwd });
	if (result.error) return new Map();
	const map = new Map<string, string>();
	for (const line of result.stdout.split("\n")) {
		if (!line) continue;
		const [ref, sha] = line.split("\t");
		if (ref && sha) map.set(ref, sha);
	}
	return map;
}

export async function fetch(params: FetchParams, cwd?: string): Promise<ToolResult<FetchResult> | ToolError> {
	const remote = params.remote ?? "origin";
	const before = await getRemoteRefs(remote, params.branch, cwd);

	const args = ["fetch"];
	if (params.all) {
		args.push("--all");
	} else {
		args.push(remote);
		if (params.branch) args.push(params.branch);
	}
	if (params.prune) args.push("--prune");
	if (params.tags) args.push("--tags");

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const after = await getRemoteRefs(remote, params.branch, cwd);
	const updated: FetchResult["updated"] = [];
	for (const [ref, newSha] of after.entries()) {
		const oldSha = before.get(ref);
		if (!oldSha || oldSha !== newSha) {
			updated.push({ ref, oldSha: oldSha ?? "", newSha });
		}
	}
	const pruned = params.prune ? Array.from(before.keys()).filter((ref) => !after.has(ref)) : undefined;

	const data: FetchResult = { updated, ...(pruned && pruned.length > 0 ? { pruned } : {}) };
	return { data, _rendered: renderFetch(data) };
}
