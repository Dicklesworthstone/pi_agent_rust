import type {
	AddResult,
	BlameResult,
	BranchListResult,
	CherryPickResult,
	CommitResult,
	DiffResult,
	FetchResult,
	LogResult,
	MergeResult,
	PullResult,
	PushResult,
	RebaseResult,
	ReleaseResult,
	RestoreResult,
	ShowCommitResult,
	ShowFileResult,
	StatusResult,
	TagResult,
} from "./types";

function renderFileList(title: string, files: string[]): string {
	if (files.length === 0) return "";
	return `${title}:\n${files.map((file) => `- ${file}`).join("\n")}`;
}

export function renderStatus(result: StatusResult): string {
	const parts: string[] = [];
	const sync = result.upstream
		? `(upstream ${result.upstream}, ahead ${result.ahead}, behind ${result.behind})`
		: "(no upstream)";
	parts.push(`Branch: ${result.branch} ${sync}`);

	if (result.staged.length > 0) {
		parts.push(
			`Staged (${result.staged.length}):\n${result.staged.map((file) => `- ${file.path} (${file.status})`).join("\n")}`,
		);
	}
	if (result.modified.length > 0) {
		parts.push(
			`Modified (${result.modified.length}):\n${result.modified.map((file) => `- ${file.path} (${file.status})`).join("\n")}`,
		);
	}
	if (result.untracked.length > 0) {
		parts.push(renderFileList(`Untracked (${result.untracked.length})`, result.untracked));
	}
	if (result.conflicts.length > 0) {
		parts.push(renderFileList(`Conflicts (${result.conflicts.length})`, result.conflicts));
	}
	if (result.ignored && result.ignored.length > 0) {
		parts.push(renderFileList(`Ignored (${result.ignored.length})`, result.ignored));
	}

	return parts.join("\n\n");
}

export function renderDiff(result: DiffResult): string {
	const header = `${result.stats.filesChanged} files changed, ${result.stats.insertions} insertions(+), ${result.stats.deletions} deletions(-)`;
	const files = result.files.map((file) => {
		const status = file.status.toUpperCase();
		return `- ${status} ${file.path} (+${file.additions}/-${file.deletions})`;
	});
	const truncatedNote = result.truncated ? "\n\nDiff output truncated." : "";
	return [header, ...files].join("\n") + truncatedNote;
}

export function renderLog(result: LogResult): string {
	if (result.commits.length === 0) return "No commits found.";
	const lines = result.commits.map((commit) => `${commit.shortSha} ${commit.subject}`);
	const more = result.hasMore ? "\n(more commits available)" : "";
	return lines.join("\n") + more;
}

export function renderShowCommit(result: ShowCommitResult): string {
	const commit = result.commit;
	const header = `${commit.shortSha} ${commit.subject}`;
	if (!result.diff) return header;
	return `${header}\n\n${renderDiff(result.diff)}`;
}

export function renderShowFile(result: ShowFileResult): string {
	const truncated = result.truncated ? "\n\n[truncated]" : "";
	return `${result.content}${truncated}`;
}

export function renderBlame(result: BlameResult): string {
	return result.lines.map((line) => `${line.shortSha} ${line.lineNo} ${line.author}: ${line.content}`).join("\n");
}

export function renderBranchList(result: BranchListResult): string {
	const lines: string[] = [];
	lines.push(`Current: ${result.current}`);
	lines.push("Local:");
	for (const branch of result.local) {
		const track = branch.upstream
			? ` (${branch.upstream} ahead ${branch.ahead ?? 0} behind ${branch.behind ?? 0}${branch.gone ? ", gone" : ""})`
			: "";
		lines.push(`- ${branch.name}${track}`);
	}
	if (result.remote && result.remote.length > 0) {
		lines.push("Remote:");
		for (const branch of result.remote) {
			lines.push(`- ${branch.name}`);
		}
	}
	return lines.join("\n");
}

export function renderAdd(result: AddResult): string {
	return `Staged ${result.staged.length} files`;
}

export function renderRestore(result: RestoreResult): string {
	return `Restored ${result.restored.length} files`;
}

export function renderCommit(result: CommitResult): string {
	return `${result.shortSha} ${result.subject}`;
}

export function renderMerge(result: MergeResult): string {
	if (result.status === "conflict") {
		return `Merge conflict${result.conflicts?.length ? `: ${result.conflicts.join(", ")}` : ""}`;
	}
	return `Merge ${result.status}`;
}

export function renderRebase(result: RebaseResult): string {
	if (result.status === "conflict") {
		return `Rebase conflict${result.conflicts?.length ? `: ${result.conflicts.join(", ")}` : ""}`;
	}
	return `Rebase ${result.status}`;
}

export function renderCherryPick(result: CherryPickResult): string {
	if (result.status === "conflict") {
		return `Cherry-pick conflict${result.conflicts?.length ? `: ${result.conflicts.join(", ")}` : ""}`;
	}
	return `Cherry-pick applied ${result.appliedCommits?.length ?? 0} commits`;
}

export function renderFetch(result: FetchResult): string {
	const lines = result.updated.map((entry) => `- ${entry.ref} ${entry.oldSha} -> ${entry.newSha}`);
	if (result.pruned && result.pruned.length > 0) {
		lines.push(`Pruned: ${result.pruned.join(", ")}`);
	}
	return lines.length > 0 ? lines.join("\n") : "Fetch completed";
}

export function renderPull(result: PullResult): string {
	if (result.status === "conflict") {
		return `Pull conflict${result.conflicts?.length ? `: ${result.conflicts.join(", ")}` : ""}`;
	}
	if (result.status === "up-to-date") return "Already up to date.";
	return `Pulled ${result.commits ?? 0} commits`;
}

export function renderPush(result: PushResult): string {
	return `Pushed ${result.commits} commits to ${result.remote}/${result.branch}`;
}

export function renderRelease(result: ReleaseResult): string {
	if ("releases" in result) {
		return `Releases: ${result.releases.length}`;
	}
	if ("tag" in result) {
		return `Release ${result.tag}`;
	}
	return "Release operation completed";
}

export function renderTag(result: TagResult): string {
	if ("tags" in result) {
		return `Tags: ${result.tags.length}`;
	}
	return "Tag operation completed";
}
