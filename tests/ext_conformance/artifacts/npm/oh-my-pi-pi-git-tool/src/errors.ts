export enum GitErrorCode {
	NOT_A_REPO = "NOT_A_REPO",
	CONFLICT = "CONFLICT",
	UNCOMMITTED_CHANGES = "UNCOMMITTED_CHANGES",
	BRANCH_NOT_FOUND = "BRANCH_NOT_FOUND",
	REF_NOT_FOUND = "REF_NOT_FOUND",
	REMOTE_REJECTED = "REMOTE_REJECTED",
	AUTH_FAILED = "AUTH_FAILED",
	MERGE_CONFLICT = "MERGE_CONFLICT",
	REBASE_CONFLICT = "REBASE_CONFLICT",
	NOTHING_TO_COMMIT = "NOTHING_TO_COMMIT",
	DETACHED_HEAD = "DETACHED_HEAD",
	HOOK_FAILED = "HOOK_FAILED",
	GH_NOT_INSTALLED = "GH_NOT_INSTALLED",
	GH_AUTH_REQUIRED = "GH_AUTH_REQUIRED",
	UNKNOWN = "UNKNOWN",
}

export class GitError extends Error {
	code: GitErrorCode;
	details?: Record<string, unknown>;

	constructor(message: string, code: GitErrorCode, details?: Record<string, unknown>) {
		super(message);
		this.name = "GitError";
		this.code = code;
		this.details = details;
	}
}

export function detectGitError(stderr: string, exitCode: number): GitError | null {
	if (exitCode === 0) return null;

	const normalized = stderr.toLowerCase();
	if (normalized.includes("not a git repository")) {
		return new GitError("Not a git repository", GitErrorCode.NOT_A_REPO);
	}
	if (normalized.includes("authentication failed") || normalized.includes("fatal: authentication")) {
		return new GitError("Authentication failed", GitErrorCode.AUTH_FAILED);
	}
	if (normalized.includes("permission denied") || normalized.includes("access denied")) {
		return new GitError("Authentication failed", GitErrorCode.AUTH_FAILED);
	}
	if (normalized.includes("nothing to commit")) {
		return new GitError("Nothing to commit", GitErrorCode.NOTHING_TO_COMMIT);
	}
	if (normalized.includes("detached head")) {
		return new GitError("Detached HEAD", GitErrorCode.DETACHED_HEAD);
	}
	if (normalized.includes("merge conflict") || normalized.includes("conflict")) {
		return new GitError("Merge conflict", GitErrorCode.MERGE_CONFLICT);
	}
	if (normalized.includes("rebase")) {
		return new GitError("Rebase conflict", GitErrorCode.REBASE_CONFLICT);
	}
	if (normalized.includes("unknown revision") || normalized.includes("bad revision")) {
		return new GitError("Ref not found", GitErrorCode.REF_NOT_FOUND);
	}
	if (normalized.includes("pathspec") && normalized.includes("did not match")) {
		return new GitError("Ref not found", GitErrorCode.REF_NOT_FOUND);
	}
	if (normalized.includes("hook") && normalized.includes("failed")) {
		return new GitError("Hook failed", GitErrorCode.HOOK_FAILED);
	}
	if (normalized.includes("remote rejected") || normalized.includes("rejected")) {
		return new GitError("Remote rejected", GitErrorCode.REMOTE_REJECTED);
	}

	return new GitError(stderr.trim() || "Unknown git error", GitErrorCode.UNKNOWN);
}

export function detectGhError(stderr: string, exitCode: number): GitError | null {
	if (exitCode === 0) return null;

	const normalized = stderr.toLowerCase();
	if (normalized.includes("not logged") || normalized.includes("authentication required")) {
		return new GitError("GitHub CLI authentication required", GitErrorCode.GH_AUTH_REQUIRED);
	}
	if (normalized.includes("gh: not found") || normalized.includes("gh: command not found")) {
		return new GitError("GitHub CLI is not installed", GitErrorCode.GH_NOT_INSTALLED);
	}

	return new GitError(stderr.trim() || "GitHub CLI error", GitErrorCode.UNKNOWN);
}
