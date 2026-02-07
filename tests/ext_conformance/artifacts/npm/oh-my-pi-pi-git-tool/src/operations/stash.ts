import type { StashEntry, StashParams, StashResult, ToolError, ToolResult } from "../types";
import { git, parseShortstat } from "../utils";

function parseStashIndex(ref: string): number {
	const match = ref.match(/stash@\{(\d+)\}/);
	return match ? Number.parseInt(match[1], 10) : -1;
}

function parseBranch(subject: string): string {
	const match = subject.match(/on ([^:]+):/i);
	return match ? match[1] : "";
}

export async function stash(params: StashParams, cwd?: string): Promise<ToolResult<StashResult> | ToolError> {
	const action = params.action ?? "list";

	if (action === "list") {
		const result = await git(["stash", "list", "--date=iso-strict", "--format=%gd%x00%gs%x00%cd"], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const stashes: StashEntry[] = [];
		for (const line of result.stdout.split("\n")) {
			if (!line) continue;
			const parts = line.split("\x00");
			if (parts.length < 3) continue;
			const ref = parts[0];
			const message = parts[1];
			const date = parts[2];
			stashes.push({
				index: parseStashIndex(ref),
				message,
				branch: parseBranch(message),
				date,
			});
		}
		return { data: { stashes }, _rendered: `Stashes: ${stashes.length}` };
	}

	if (action === "show") {
		const index = params.index ?? 0;
		const ref = `stash@{${index}}`;
		const statResult = await git(["stash", "show", "--shortstat", ref], { cwd });
		if (statResult.error) {
			return { error: statResult.error.message, code: statResult.error.code };
		}
		const statLine = statResult.stdout.split("\n").find((line) => line.includes("files changed"));
		const statsParsed = statLine ? parseShortstat(statLine) : null;
		const stats = {
			files: statsParsed?.files ?? 0,
			additions: statsParsed?.additions ?? 0,
			deletions: statsParsed?.deletions ?? 0,
		};
		const filesResult = await git(["stash", "show", "--name-only", ref], { cwd });
		if (filesResult.error) {
			return { error: filesResult.error.message, code: filesResult.error.code };
		}
		const files = filesResult.stdout.split("\n").filter(Boolean);
		return { data: { stats, files }, _rendered: `Stash ${index} (${files.length} files)` };
	}

	if (action === "push") {
		const args = ["stash", "push"];
		if (params.message) args.push("-m", params.message);
		if (params.include_untracked) args.push("-u");
		if (params.keep_index) args.push("--keep-index");
		const result = await git(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		return { data: { status: "success" }, _rendered: "Stash saved" };
	}

	if (action === "pop" || action === "apply" || action === "drop") {
		const index = params.index ?? 0;
		const ref = `stash@{${index}}`;
		const args = ["stash", action, ref];
		const result = await git(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		return { data: { status: "success" }, _rendered: `Stash ${action} ${index}` };
	}

	return { error: `Unknown stash action: ${action}` };
}
