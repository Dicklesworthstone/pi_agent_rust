import { parseDiff } from "../parsers/diff-parser";
import { renderDiff } from "../render";
import type { DiffParams, DiffResult, FileDiff, ToolError, ToolResult } from "../types";
import { git } from "../utils";

interface StatusInfo {
	status: FileDiff["status"];
	oldPath?: string;
}

function parseNameStatus(output: string): Map<string, StatusInfo> {
	const map = new Map<string, StatusInfo>();
	for (const line of output.split("\n")) {
		if (!line) continue;
		const parts = line.split("\t");
		const code = parts[0];
		if (!code) continue;
		const status = code[0];
		if (status === "R" || status === "C") {
			const oldPath = parts[1];
			const newPath = parts[2];
			if (newPath) {
				map.set(newPath, {
					status: status === "R" ? "renamed" : "copied",
					oldPath,
				});
			}
			continue;
		}
		const path = parts[1];
		if (!path) continue;
		const statusMap: Record<string, FileDiff["status"]> = {
			A: "added",
			M: "modified",
			D: "deleted",
		};
		map.set(path, { status: statusMap[status] ?? "modified" });
	}
	return map;
}

function parseRenamePath(path: string): { oldPath: string; newPath: string } | null {
	if (!path.includes("=>")) return null;
	const braceMatch = path.match(/^(.*)\{(.*) => (.*)\}(.*)$/);
	if (braceMatch) {
		const prefix = braceMatch[1];
		const oldMid = braceMatch[2];
		const newMid = braceMatch[3];
		const suffix = braceMatch[4];
		return {
			oldPath: `${prefix}${oldMid}${suffix}`,
			newPath: `${prefix}${newMid}${suffix}`,
		};
	}
	const parts = path.split("=>").map((part) => part.trim());
	if (parts.length !== 2) return null;
	return { oldPath: parts[0], newPath: parts[1] };
}

function parseNumstat(
	output: string,
): Map<string, { additions: number; deletions: number; binary: boolean; oldPath?: string }> {
	const map = new Map<string, { additions: number; deletions: number; binary: boolean; oldPath?: string }>();
	for (const line of output.split("\n")) {
		if (!line) continue;
		const parts = line.split("\t");
		if (parts.length < 3) continue;
		const additionsRaw = parts[0];
		const deletionsRaw = parts[1];
		const pathRaw = parts.slice(2).join("\t");
		const additions = additionsRaw === "-" ? 0 : Number.parseInt(additionsRaw, 10);
		const deletions = deletionsRaw === "-" ? 0 : Number.parseInt(deletionsRaw, 10);
		const binary = additionsRaw === "-" || deletionsRaw === "-";
		const rename = parseRenamePath(pathRaw);
		if (rename) {
			map.set(rename.newPath, { additions, deletions, binary, oldPath: rename.oldPath });
			continue;
		}
		map.set(pathRaw, { additions, deletions, binary });
	}
	return map;
}

function buildDiffArgs(params: DiffParams): string[] {
	const args: string[] = ["diff"];
	if (params.target === "staged") {
		args.push("--cached");
	} else if (params.target === "head") {
		args.push("HEAD");
	} else if (params.target && typeof params.target === "object") {
		// Ambiguity: when `to` is omitted, default to diffing against HEAD.
		const toRef = params.target.to ?? "HEAD";
		args.push(`${params.target.from}..${toRef}`);
	}

	if (params.ignore_whitespace) {
		args.push("--ignore-all-space");
	}
	return args;
}

export async function diff(params: DiffParams, cwd?: string): Promise<ToolResult<DiffResult> | ToolError> {
	const baseArgs = buildDiffArgs(params);
	const paths = params.paths ?? [];

	const nameStatusArgs = [...baseArgs, "--name-status"];
	if (paths.length > 0) nameStatusArgs.push("--", ...paths);
	const nameStatusResult = await git(nameStatusArgs, { cwd });
	if (nameStatusResult.error) {
		return { error: nameStatusResult.error.message, code: nameStatusResult.error.code };
	}
	const statusMap = parseNameStatus(nameStatusResult.stdout);

	if (params.name_only || params.stat_only) {
		const numstatArgs = [...baseArgs, "--numstat"];
		if (paths.length > 0) numstatArgs.push("--", ...paths);
		const numstatResult = await git(numstatArgs, { cwd });
		if (numstatResult.error) {
			return { error: numstatResult.error.message, code: numstatResult.error.code };
		}
		const statsMap = parseNumstat(numstatResult.stdout);
		const files: FileDiff[] = [];
		const seen = new Set<string>();

		for (const [path, info] of statusMap.entries()) {
			const stats = statsMap.get(path);
			files.push({
				path,
				oldPath: info.oldPath ?? stats?.oldPath,
				status: info.status,
				binary: stats?.binary ?? false,
				additions: stats?.additions ?? 0,
				deletions: stats?.deletions ?? 0,
			});
			seen.add(path);
		}

		for (const [path, stats] of statsMap.entries()) {
			if (seen.has(path)) continue;
			files.push({
				path,
				oldPath: stats.oldPath,
				status: "modified",
				binary: stats.binary,
				additions: stats.additions,
				deletions: stats.deletions,
			});
		}

		const summary = files.reduce(
			(acc, file) => {
				acc.filesChanged += 1;
				acc.insertions += file.additions;
				acc.deletions += file.deletions;
				return acc;
			},
			{ filesChanged: 0, insertions: 0, deletions: 0 },
		);

		const result: DiffResult = {
			files,
			stats: summary,
			truncated: false,
		};
		return { data: result, _rendered: renderDiff(result) };
	}

	const diffArgs = [...baseArgs];
	if (params.context !== undefined) {
		diffArgs.push(`--unified=${params.context}`);
	}
	if (paths.length > 0) diffArgs.push("--", ...paths);

	const diffResult = await git(diffArgs, { cwd });
	if (diffResult.error) {
		return { error: diffResult.error.message, code: diffResult.error.code };
	}

	const parsed = parseDiff(diffResult.stdout, { maxLines: params.max_lines });
	for (const file of parsed.files) {
		const info = statusMap.get(file.path);
		if (info) {
			file.status = info.status;
			file.oldPath = info.oldPath ?? file.oldPath;
		}
	}

	const summary = parsed.files.reduce(
		(acc, file) => {
			acc.filesChanged += 1;
			acc.insertions += file.additions;
			acc.deletions += file.deletions;
			return acc;
		},
		{ filesChanged: 0, insertions: 0, deletions: 0 },
	);

	const result: DiffResult = {
		files: parsed.files,
		stats: summary,
		truncated: parsed.truncated,
		truncatedFiles: parsed.truncatedFiles.length > 0 ? parsed.truncatedFiles : undefined,
	};

	return { data: result, _rendered: renderDiff(result) };
}
