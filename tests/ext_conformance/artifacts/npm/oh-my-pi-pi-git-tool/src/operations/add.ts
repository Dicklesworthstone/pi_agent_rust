import { renderAdd } from "../render";
import type { AddParams, AddResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

function parseDryRun(output: string): string[] {
	const files: string[] = [];
	for (const line of output.split("\n")) {
		const match = line.match(/add ['"]?(.*?)['"]?$/i);
		if (match) {
			files.push(match[1]);
		}
	}
	return files;
}

export async function add(params: AddParams, cwd?: string): Promise<ToolResult<AddResult> | ToolError> {
	const args = ["add"];
	if (params.dry_run) args.push("--dry-run");
	if (params.update) args.push("-u");
	if (params.all) args.push("-A");
	if (params.paths && params.paths.length > 0) {
		args.push("--", ...params.paths);
	}

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	let staged: string[] = [];
	if (params.dry_run) {
		staged = parseDryRun(result.stdout);
	} else {
		const stagedResult = await git(["diff", "--name-only", "--cached"], { cwd });
		if (!stagedResult.error) {
			staged = stagedResult.stdout.split("\n").filter(Boolean);
		}
	}

	const data: AddResult = { staged };
	return { data, _rendered: renderAdd(data) };
}
