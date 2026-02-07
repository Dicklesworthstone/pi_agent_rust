import { renderRestore } from "../render";
import type { RestoreParams, RestoreResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

export async function restore(params: RestoreParams, cwd?: string): Promise<ToolResult<RestoreResult> | ToolError> {
	const args = ["restore"];
	if (params.staged) args.push("--staged");
	if (params.worktree) args.push("--worktree");
	if (params.source) args.push(`--source=${params.source}`);
	args.push("--", ...params.paths);

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const data: RestoreResult = { restored: params.paths };
	return { data, _rendered: renderRestore(data) };
}
