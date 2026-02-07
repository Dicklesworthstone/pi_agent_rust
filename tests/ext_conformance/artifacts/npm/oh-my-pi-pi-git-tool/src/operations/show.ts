import { parseDiff } from "../parsers/diff-parser";
import { parseLog } from "../parsers/log-parser";
import { renderShowCommit, renderShowFile } from "../render";
import type { ShowCommitResult, ShowFileResult, ShowParams, ToolError, ToolResult } from "../types";
import { git } from "../utils";

const LOG_FORMAT = "%H%x00%h%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%P%x00%s%x00%b%x1e";

const MAX_SHOW_LINES = 2000;
const MAX_SHOW_BYTES = 200_000;

export async function show(
	params: ShowParams,
	cwd?: string,
): Promise<ToolResult<ShowCommitResult | ShowFileResult> | ToolError> {
	if (params.path) {
		const result = await git(["show", `${params.ref}:${params.path}`], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}

		let content = result.stdout;
		let truncated = false;

		// Ambiguity: no truncation limits specified for show file; defaulting to 2000 lines or 200KB.
		if (content.length > MAX_SHOW_BYTES) {
			content = content.slice(0, MAX_SHOW_BYTES);
			truncated = true;
		}

		let lines = content.split("\n");
		if (lines.length > MAX_SHOW_LINES) {
			lines = lines.slice(0, MAX_SHOW_LINES);
			content = lines.join("\n");
			truncated = true;
		}

		if (params.lines) {
			const start = Math.max(1, params.lines.start);
			const end = Math.max(start, params.lines.end);
			const slice = lines.slice(start - 1, end);
			content = slice.join("\n");
			truncated = false;
		}

		const data: ShowFileResult = {
			path: params.path,
			ref: params.ref,
			content,
			truncated,
		};
		return { data, _rendered: renderShowFile(data) };
	}

	const commitResult = await git(["show", "-s", `--format=${LOG_FORMAT}`, params.ref], { cwd });
	if (commitResult.error) {
		return { error: commitResult.error.message, code: commitResult.error.code };
	}
	const commits = parseLog(commitResult.stdout);
	const commit = commits[0];
	if (!commit) {
		return { error: "Commit not found", code: "REF_NOT_FOUND" };
	}

	let diffData: ShowCommitResult["diff"] | undefined;
	if (params.diff || params.stat) {
		const diffResult = await git(["show", params.ref, "--format="], { cwd });
		if (diffResult.error) {
			return { error: diffResult.error.message, code: diffResult.error.code };
		}
		const parsed = parseDiff(diffResult.stdout);
		const files = parsed.files.map((file) => {
			if (!params.diff) {
				delete file.hunks;
			}
			return file;
		});
		const stats = files.reduce(
			(acc, file) => {
				acc.filesChanged += 1;
				acc.insertions += file.additions;
				acc.deletions += file.deletions;
				return acc;
			},
			{ filesChanged: 0, insertions: 0, deletions: 0 },
		);
		const diff: ShowCommitResult["diff"] = {
			files,
			stats,
			truncated: parsed.truncated,
			truncatedFiles: parsed.truncatedFiles.length > 0 ? parsed.truncatedFiles : undefined,
		};
		diffData = diff;
	}

	const data: ShowCommitResult = {
		commit,
		...(diffData ? { diff: diffData } : {}),
	};

	return { data, _rendered: renderShowCommit(data) };
}
