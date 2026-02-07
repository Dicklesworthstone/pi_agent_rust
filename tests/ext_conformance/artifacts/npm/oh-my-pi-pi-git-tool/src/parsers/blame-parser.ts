import type { BlameLine } from "../types";

interface BlameMeta {
	sha: string;
	shortSha: string;
	author: string;
	date: string;
	lineNo: number;
	original?: { sha: string; path: string; lineNo: number };
}

function formatDate(timestamp: string): string {
	const seconds = Number.parseInt(timestamp, 10);
	if (!Number.isFinite(seconds)) return "";
	return new Date(seconds * 1000).toISOString();
}

export function parseBlame(output: string): BlameLine[] {
	const lines = output.split("\n");
	const result: BlameLine[] = [];
	let current: BlameMeta | null = null;
	let remaining = 0;
	let currentFilename = "";

	for (const line of lines) {
		if (!line) continue;
		if (line.startsWith("\t")) {
			if (!current) continue;
			result.push({
				lineNo: current.lineNo,
				sha: current.sha,
				shortSha: current.shortSha,
				author: current.author,
				date: current.date,
				content: line.slice(1),
				original: current.original,
			});
			current.lineNo += 1;
			remaining -= 1;
			if (remaining <= 0) {
				current = null;
			}
			continue;
		}

		const headerMatch = line.match(/^([0-9a-f]{40}) (\d+) (\d+) (\d+)/);
		if (headerMatch) {
			const sha = headerMatch[1];
			const finalLine = Number.parseInt(headerMatch[3], 10);
			remaining = Number.parseInt(headerMatch[4], 10);
			current = {
				sha,
				shortSha: sha.slice(0, 7),
				author: "",
				date: "",
				lineNo: finalLine,
			};
			continue;
		}

		if (!current) continue;

		if (line.startsWith("author ")) {
			current.author = line.slice(7).trim();
			continue;
		}
		if (line.startsWith("author-time ")) {
			current.date = formatDate(line.slice(12).trim());
			continue;
		}
		if (line.startsWith("previous ")) {
			const parts = line.split(" ");
			if (parts.length >= 3) {
				current.original = {
					sha: parts[1],
					path: currentFilename,
					lineNo: current.lineNo,
				};
			}
			continue;
		}
		if (line.startsWith("filename ")) {
			currentFilename = line.slice(9).trim();
			if (current.original) {
				current.original.path = currentFilename;
			}
		}
	}

	return result;
}
