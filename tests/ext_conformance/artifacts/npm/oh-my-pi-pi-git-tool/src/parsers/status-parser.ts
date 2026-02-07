import type { FileStatus, StatusResult } from "../types";

function mapStatus(code: string, path: string, oldPath?: string): FileStatus {
	const statusMap: Record<string, FileStatus["status"]> = {
		A: "added",
		M: "modified",
		D: "deleted",
		R: "renamed",
		C: "copied",
		T: "modified",
		U: "modified",
	};
	return { path, status: statusMap[code] ?? "modified", oldPath };
}

export function parseStatus(output: string, includeIgnored: boolean): StatusResult {
	const lines = output.split("\n");
	const result: StatusResult = {
		branch: "",
		upstream: null,
		ahead: 0,
		behind: 0,
		staged: [],
		modified: [],
		untracked: [],
		conflicts: [],
	};
	if (includeIgnored) {
		result.ignored = [];
	}

	for (const line of lines) {
		if (line.startsWith("# branch.head ")) {
			result.branch = line.slice(14).trim();
			continue;
		}
		if (line.startsWith("# branch.upstream ")) {
			result.upstream = line.slice(18).trim();
			continue;
		}
		if (line.startsWith("# branch.ab ")) {
			const match = line.match(/\+(\d+) -?(\d+)/);
			if (match) {
				result.ahead = Number.parseInt(match[1], 10);
				result.behind = Number.parseInt(match[2] ?? "0", 10);
			}
			continue;
		}

		if (line.startsWith("1 ") || line.startsWith("2 ")) {
			const parts = line.split(" ");
			const xy = parts[1];
			let path = parts.slice(-1)[0] ?? "";
			let oldPath: string | undefined;

			if (line.startsWith("2 ")) {
				const [beforeTab, afterTab] = line.split("\t");
				if (afterTab) {
					const preParts = beforeTab.split(" ");
					path = preParts[preParts.length - 1];
					oldPath = afterTab.trim();
				}
			}

			if (xy[0] !== ".") {
				result.staged.push(mapStatus(xy[0], path, oldPath));
			}
			if (xy[1] !== ".") {
				result.modified.push(mapStatus(xy[1], path, oldPath));
			}
			continue;
		}

		if (line.startsWith("u ")) {
			const path = line.split(" ").slice(-1)[0];
			if (path) {
				result.conflicts.push(path);
			}
			continue;
		}

		if (line.startsWith("? ")) {
			result.untracked.push(line.slice(2));
			continue;
		}

		if (includeIgnored && line.startsWith("! ")) {
			result.ignored?.push(line.slice(2));
		}
	}

	return result;
}
