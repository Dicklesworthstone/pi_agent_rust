import type { DiffLine, FileDiff, Hunk } from "../types";

export interface DiffParseResult {
	files: FileDiff[];
	truncated: boolean;
	truncatedFiles: string[];
}

export interface DiffParseOptions {
	maxLines?: number;
}

function parseHunkHeader(header: string): { oldStart: number; oldCount: number; newStart: number; newCount: number } {
	const match = header.match(/@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
	if (!match) {
		return { oldStart: 0, oldCount: 0, newStart: 0, newCount: 0 };
	}
	return {
		oldStart: Number.parseInt(match[1], 10),
		oldCount: Number.parseInt(match[2] ?? "1", 10),
		newStart: Number.parseInt(match[3], 10),
		newCount: Number.parseInt(match[4] ?? "1", 10),
	};
}

export function parseDiff(raw: string, options?: DiffParseOptions): DiffParseResult {
	const maxLines = options?.maxLines ?? Number.POSITIVE_INFINITY;
	const lines = raw.split("\n");
	const files: FileDiff[] = [];
	const truncatedFiles: string[] = [];
	let truncated = false;
	let lineCount = 0;

	let current: FileDiff | null = null;
	let currentHunk: Hunk | null = null;
	let oldLine = 0;
	let newLine = 0;

	const finalizeHunk = () => {
		if (current && currentHunk) {
			current.hunks = current.hunks ?? [];
			current.hunks.push(currentHunk);
			currentHunk = null;
		}
	};

	const finalizeFile = () => {
		finalizeHunk();
		if (current) {
			files.push(current);
			current = null;
		}
	};

	for (const line of lines) {
		if (line.startsWith("diff --git ")) {
			finalizeFile();
			const match = line.match(/^diff --git a\/(.*) b\/(.*)$/);
			const path = match ? match[2] : line.slice("diff --git ".length).trim();
			const oldPath = match ? match[1] : undefined;
			current = {
				path,
				oldPath,
				status: "modified",
				binary: false,
				additions: 0,
				deletions: 0,
			};
			continue;
		}

		if (!current) continue;

		if (line.startsWith("new file mode")) {
			current.status = "added";
			continue;
		}
		if (line.startsWith("deleted file mode")) {
			current.status = "deleted";
			continue;
		}
		if (line.startsWith("rename from ")) {
			current.status = "renamed";
			current.oldPath = line.slice("rename from ".length).trim();
			continue;
		}
		if (line.startsWith("rename to ")) {
			current.path = line.slice("rename to ".length).trim();
			continue;
		}
		if (line.startsWith("copy from ")) {
			current.status = "copied";
			current.oldPath = line.slice("copy from ".length).trim();
			continue;
		}
		if (line.startsWith("copy to ")) {
			current.path = line.slice("copy to ".length).trim();
			continue;
		}
		if (line.startsWith("Binary files ") || line.startsWith("GIT binary patch")) {
			current.binary = true;
			continue;
		}

		if (line.startsWith("@@ ")) {
			finalizeHunk();
			const header = line;
			const ranges = parseHunkHeader(header);
			currentHunk = {
				oldStart: ranges.oldStart,
				oldCount: ranges.oldCount,
				newStart: ranges.newStart,
				newCount: ranges.newCount,
				header,
				lines: [],
			};
			oldLine = ranges.oldStart;
			newLine = ranges.newStart;
			continue;
		}

		if (!currentHunk) {
			continue;
		}

		if (lineCount >= maxLines) {
			if (!truncated) {
				truncated = true;
				truncatedFiles.push(current.path);
			}
			continue;
		}

		if (line.startsWith("\\")) {
			continue;
		}

		let diffLine: DiffLine | null = null;
		if (line.startsWith("+") && !line.startsWith("+++")) {
			diffLine = { type: "add", content: line.slice(1), newLineNo: newLine };
			newLine += 1;
			current.additions += 1;
		} else if (line.startsWith("-") && !line.startsWith("---")) {
			diffLine = { type: "delete", content: line.slice(1), oldLineNo: oldLine };
			oldLine += 1;
			current.deletions += 1;
		} else if (line.startsWith(" ")) {
			diffLine = { type: "context", content: line.slice(1), oldLineNo: oldLine, newLineNo: newLine };
			oldLine += 1;
			newLine += 1;
		}

		if (diffLine) {
			currentHunk.lines.push(diffLine);
			lineCount += 1;
		}
	}

	finalizeFile();

	return { files, truncated, truncatedFiles };
}
