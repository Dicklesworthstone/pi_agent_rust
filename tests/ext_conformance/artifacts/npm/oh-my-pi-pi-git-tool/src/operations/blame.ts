import { parseBlame } from "../parsers/blame-parser";
import { renderBlame } from "../render";
import type { BlameParams, BlameResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

export async function blame(params: BlameParams, cwd?: string): Promise<ToolResult<BlameResult> | ToolError> {
	const args = ["blame", "--porcelain"];
	if (params.root) args.push("--root");
	if (params.ignore_whitespace) args.push("-w");
	if (params.lines) {
		args.push("-L", `${params.lines.start},${params.lines.end}`);
	}
	args.push(params.path);

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const parsed = parseBlame(result.stdout);
	const data: BlameResult = { lines: parsed };
	return { data, _rendered: renderBlame(data) };
}
