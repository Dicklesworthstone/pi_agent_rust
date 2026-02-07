import { renderBranchList } from "../render";
import type { BranchInfo, BranchListResult, BranchParams, ToolError, ToolResult } from "../types";
import { git } from "../utils";

function parseTrack(track: string): { ahead?: number; behind?: number; gone?: boolean } {
	const result: { ahead?: number; behind?: number; gone?: boolean } = {};
	if (!track) return result;
	if (track.includes("gone")) result.gone = true;
	const aheadMatch = track.match(/ahead (\d+)/);
	const behindMatch = track.match(/behind (\d+)/);
	if (aheadMatch) result.ahead = Number.parseInt(aheadMatch[1], 10);
	if (behindMatch) result.behind = Number.parseInt(behindMatch[1], 10);
	return result;
}

function parseBranchLines(output: string): BranchInfo[] {
	const branches: BranchInfo[] = [];
	for (const line of output.split("\n")) {
		if (!line) continue;
		const parts = line.split("\x00");
		if (parts.length < 2) continue;
		const name = parts[0];
		const sha = parts[1];
		const upstream = parts[2] || undefined;
		const track = parts[3] || "";
		const { ahead, behind, gone } = parseTrack(track);
		branches.push({ name, sha, upstream, ahead, behind, gone });
	}
	return branches;
}

export async function branch(params: BranchParams, cwd?: string): Promise<ToolResult<BranchListResult> | ToolError> {
	const action = params.action ?? "list";

	if (action === "current") {
		const currentResult = await git(["branch", "--show-current"], { cwd });
		if (currentResult.error) {
			return { error: currentResult.error.message, code: currentResult.error.code };
		}
		const data: BranchListResult = { current: currentResult.stdout.trim(), local: [] };
		return { data, _rendered: renderBranchList(data) };
	}

	if (action === "create") {
		if (!params.name) {
			return { error: "Branch name required" };
		}
		const args = ["branch", params.name];
		if (params.startPoint) args.push(params.startPoint);
		const result = await git(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
	}

	if (action === "delete") {
		if (!params.name) {
			return { error: "Branch name required" };
		}
		const args = ["branch", params.force ? "-D" : "-d", params.name];
		const result = await git(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
	}

	if (action === "rename") {
		if (!params.name || !params.newName) {
			return { error: "Branch name and newName required" };
		}
		const result = await git(["branch", "-m", params.name, params.newName], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
	}

	const currentResult = await git(["branch", "--show-current"], { cwd });
	if (currentResult.error) {
		return { error: currentResult.error.message, code: currentResult.error.code };
	}
	const current = currentResult.stdout.trim();

	const listResult = await git(
		[
			"branch",
			"-vv",
			"--format=%(refname:short)%x00%(objectname:short)%x00%(upstream:short)%x00%(upstream:track,nobracket)",
		],
		{ cwd },
	);
	if (listResult.error) {
		return { error: listResult.error.message, code: listResult.error.code };
	}
	const local = parseBranchLines(listResult.stdout);

	let remote: BranchInfo[] | undefined;
	if (params.remotes) {
		const remoteResult = await git(
			[
				"branch",
				"-r",
				"-vv",
				"--format=%(refname:short)%x00%(objectname:short)%x00%(upstream:short)%x00%(upstream:track,nobracket)",
			],
			{ cwd },
		);
		if (remoteResult.error) {
			return { error: remoteResult.error.message, code: remoteResult.error.code };
		}
		remote = parseBranchLines(remoteResult.stdout);
	}

	const data: BranchListResult = { current, local, ...(remote ? { remote } : {}) };
	return { data, _rendered: renderBranchList(data) };
}
