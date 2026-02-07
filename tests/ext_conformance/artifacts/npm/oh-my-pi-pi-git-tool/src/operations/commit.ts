import { renderCommit } from "../render";
import { markCommitCreated } from "../safety/guards";
import type { CommitParams, CommitResult, ToolError, ToolResult } from "../types";
import { git, parseShortstat } from "../utils";

export async function commit(params: CommitParams, cwd?: string): Promise<ToolResult<CommitResult> | ToolError> {
	if (!params.message || params.message.trim().length === 0) {
		return { error: "Commit message is required" };
	}
	const args = ["commit", "-m", params.message];
	if (params.all) args.push("--all");
	if (params.allow_empty) args.push("--allow-empty");
	if (params.sign) args.push("-S");
	if (params.no_verify) args.push("--no-verify");
	if (params.amend) args.push("--amend");

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const shaResult = await git(["rev-parse", "HEAD"], { cwd });
	if (shaResult.error) {
		return { error: shaResult.error.message, code: shaResult.error.code };
	}
	const sha = shaResult.stdout.trim();
	markCommitCreated(sha);

	const subjectResult = await git(["show", "-s", "--format=%s", "HEAD"], { cwd });
	if (subjectResult.error) {
		return { error: subjectResult.error.message, code: subjectResult.error.code };
	}
	const subject = subjectResult.stdout.trim();

	const statResult = await git(["show", "-s", "--shortstat", "HEAD"], { cwd });
	let stats = { additions: 0, deletions: 0, files: 0 };
	if (!statResult.error) {
		const line = statResult.stdout.split("\n").find((statLine) => statLine.includes("files changed"));
		const parsed = line ? parseShortstat(line) : null;
		if (parsed) {
			stats = { files: parsed.files, additions: parsed.additions, deletions: parsed.deletions };
		}
	}

	const data: CommitResult = {
		sha,
		shortSha: sha.slice(0, 7),
		subject,
		stats,
	};
	return { data, _rendered: renderCommit(data) };
}
