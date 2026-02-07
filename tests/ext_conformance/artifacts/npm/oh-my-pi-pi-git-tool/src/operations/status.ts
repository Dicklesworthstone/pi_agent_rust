import { parseStatus } from "../parsers/status-parser";
import { renderStatus } from "../render";
import type { StatusParams, StatusResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

export async function status(params: StatusParams, cwd?: string): Promise<ToolResult<StatusResult> | ToolError> {
	const args = ["status", "--porcelain=v2", "--branch", "--ahead-behind"];
	if (params.ignored) args.push("--ignored");

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const parsed = parseStatus(result.stdout, Boolean(params.ignored));
	let finalResult = parsed;

	if (params.only) {
		const base: StatusResult = {
			branch: parsed.branch,
			upstream: parsed.upstream,
			ahead: parsed.ahead,
			behind: parsed.behind,
			staged: [],
			modified: [],
			untracked: [],
			conflicts: [],
		};
		switch (params.only) {
			case "branch":
				finalResult = base;
				break;
			case "modified":
				finalResult = { ...base, modified: parsed.modified };
				break;
			case "staged":
				finalResult = { ...base, staged: parsed.staged };
				break;
			case "untracked":
				finalResult = { ...base, untracked: parsed.untracked };
				break;
			case "conflicts":
				finalResult = { ...base, conflicts: parsed.conflicts };
				break;
			case "sync":
				finalResult = base;
				break;
			default:
				finalResult = parsed;
		}
	}

	return { data: finalResult, _rendered: renderStatus(finalResult) };
}
