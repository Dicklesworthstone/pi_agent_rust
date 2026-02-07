import { parseLog } from "../parsers/log-parser";
import { renderLog } from "../render";
import type { Commit, LogParams, LogResult, ToolError, ToolResult } from "../types";
import { git, parseShortstat } from "../utils";

const LOG_FORMAT = "%H%x00%h%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%P%x00%s%x00%b%x1e";

async function enrichStats(commits: Commit[], cwd?: string): Promise<void> {
	for (const commit of commits) {
		const result = await git(["show", "-s", "--shortstat", commit.sha], { cwd });
		if (result.error) continue;
		const statLine = result.stdout.split("\n").find((line) => line.includes("files changed"));
		if (!statLine) continue;
		const stats = parseShortstat(statLine);
		if (stats) {
			commit.stats = {
				files: stats.files,
				additions: stats.additions,
				deletions: stats.deletions,
			};
		}
	}
}

export async function log(params: LogParams, cwd?: string): Promise<ToolResult<LogResult> | ToolError> {
	const limit = params.limit ?? 10;
	const fetchLimit = limit + 1;

	const args = ["log", `--format=${LOG_FORMAT}`, "-n", String(fetchLimit)];
	if (params.ref) args.push(params.ref);
	if (params.author) args.push(`--author=${params.author}`);
	if (params.since) args.push(`--since=${params.since}`);
	if (params.until) args.push(`--until=${params.until}`);
	if (params.grep) args.push(`--grep=${params.grep}`);
	if (params.merges === true) args.push("--merges");
	if (params.merges === false) args.push("--no-merges");
	if (params.first_parent) args.push("--first-parent");
	if (params.paths && params.paths.length > 0) {
		args.push("--", ...params.paths);
	}

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	let commits = parseLog(result.stdout);
	const hasMore = commits.length > limit;
	if (hasMore) commits = commits.slice(0, limit);

	if (params.format && params.format !== "full") {
		for (const commit of commits) {
			commit.message = commit.subject;
		}
	}

	if (params.stat) {
		await enrichStats(commits, cwd);
	}

	const finalResult: LogResult = { commits, hasMore };
	return { data: finalResult, _rendered: renderLog(finalResult) };
}
