import type { Operation, SafetyCheck, SafetyResult } from "../types";
import { git } from "../utils";
import { defaultPolicy, isProtectedBranch } from "./policies";

const sessionCommits = new Set<string>();

export function markCommitCreated(sha: string): void {
	sessionCommits.add(sha);
}

async function getCurrentBranch(cwd?: string): Promise<string> {
	const result = await git(["rev-parse", "--abbrev-ref", "HEAD"], { cwd });
	if (result.error) return "";
	return result.stdout.trim();
}

async function getHeadSha(cwd?: string): Promise<string> {
	const result = await git(["rev-parse", "HEAD"], { cwd });
	if (result.error) return "";
	return result.stdout.trim();
}

async function getBranchSync(cwd?: string): Promise<{ upstream: string | null; ahead: number; behind: number }> {
	const result = await git(["status", "--porcelain=v2", "--branch", "--ahead-behind"], { cwd });
	if (result.error) return { upstream: null, ahead: 0, behind: 0 };

	let upstream: string | null = null;
	let ahead = 0;
	let behind = 0;
	for (const line of result.stdout.split("\n")) {
		if (line.startsWith("# branch.upstream ")) {
			upstream = line.slice(18).trim();
		}
		if (line.startsWith("# branch.ab ")) {
			const match = line.match(/\+(\d+) -?(\d+)/);
			if (match) {
				ahead = Number.parseInt(match[1], 10);
				behind = Number.parseInt(match[2] ?? "0", 10);
			}
		}
	}
	return { upstream, ahead, behind };
}

async function isHeadPushed(cwd?: string): Promise<boolean> {
	const sync = await getBranchSync(cwd);
	if (!sync.upstream) return false;
	return sync.ahead === 0;
}

function evaluateChecks(checks: SafetyCheck[]): SafetyResult {
	const blocked = checks.find((check) => check.level === "block");
	if (blocked) {
		return {
			blocked: true,
			confirm: false,
			message: blocked.message,
			suggestion: blocked.suggestion,
			override: blocked.override,
			warnings: [],
		};
	}
	const confirm = checks.find((check) => check.level === "confirm");
	if (confirm) {
		return {
			blocked: false,
			confirm: true,
			message: confirm.message,
			suggestion: confirm.suggestion,
			override: confirm.override,
			warnings: [],
		};
	}
	const warnings = checks.filter((check) => check.level === "warn").map((check) => check.message);
	return {
		blocked: false,
		confirm: false,
		warnings,
	};
}

export async function checkSafety(
	operation: Operation,
	params: Record<string, unknown>,
	cwd?: string,
): Promise<SafetyResult> {
	const checks: SafetyCheck[] = [];

	if (operation === "push" && params.force) {
		const branch = (params.branch as string | undefined) ?? (await getCurrentBranch(cwd));
		if (branch && isProtectedBranch(branch)) {
			checks.push({
				level: defaultPolicy.forcePushMain,
				message: `Force push to protected branch '${branch}' is blocked`,
				override: "force_override",
			});
		} else {
			checks.push({
				level: defaultPolicy.forcePush,
				message: `Force push to '${branch || "current branch"}' will overwrite remote history`,
				suggestion: "Consider using force_with_lease instead",
			});
		}
	}

	if (operation === "commit" && params.amend) {
		const pushed = await isHeadPushed(cwd);
		const headSha = await getHeadSha(cwd);
		if (pushed && headSha && !sessionCommits.has(headSha)) {
			checks.push({
				level: defaultPolicy.amendPushed,
				message: "Cannot amend: HEAD has been pushed to remote",
				suggestion: "Create a new commit instead",
			});
		}
	}

	if (operation === "rebase") {
		const pushed = await isHeadPushed(cwd);
		if (pushed) {
			checks.push({
				level: defaultPolicy.rebasePushed,
				message: "Cannot rebase: HEAD has been pushed to remote",
				suggestion: "Merge instead of rebasing pushed commits",
			});
		}
	}

	if (operation === "restore" && params.worktree && !params.staged) {
		checks.push({
			level: defaultPolicy.discardChanges,
			message: "Restoring worktree will discard local changes",
		});
	}

	if (operation === "branch" && params.action === "delete") {
		checks.push({
			level: defaultPolicy.deleteBranch,
			message: "Deleting a branch will remove its ref",
		});
	}

	return evaluateChecks(checks);
}
