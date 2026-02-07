import type { Subprocess } from "bun";
import { detectGhError, detectGitError, GitError, GitErrorCode } from "./errors";

export interface ExecResult {
	stdout: string;
	stderr: string;
	exitCode: number;
	error?: GitError | null;
}

export interface ExecOptions {
	cwd?: string;
	signal?: AbortSignal;
	timeout?: number;
}

export async function exec(command: string, args: string[], options?: ExecOptions): Promise<ExecResult> {
	const cwd = options?.cwd ?? process.cwd();
	const proc: Subprocess = Bun.spawn([command, ...args], {
		cwd,
		stdin: "ignore",
		stdout: "pipe",
		stderr: "pipe",
	});

	let killed = false;
	let timeoutId: ReturnType<typeof setTimeout> | undefined;

	const killProcess = () => {
		if (!killed) {
			killed = true;
			proc.kill();
			setTimeout(() => {
				try {
					proc.kill(9);
				} catch {
					// Ignore if already dead.
				}
			}, 5000);
		}
	};

	if (options?.signal) {
		if (options.signal.aborted) {
			killProcess();
		} else {
			options.signal.addEventListener("abort", killProcess, { once: true });
		}
	}

	if (options?.timeout && options.timeout > 0) {
		timeoutId = setTimeout(() => {
			killProcess();
		}, options.timeout);
	}

	const [stdout, stderr, exitCode] = await Promise.all([
		(proc.stdout as ReadableStream<Uint8Array>).text(),
		(proc.stderr as ReadableStream<Uint8Array>).text(),
		proc.exited,
	]);

	if (timeoutId) clearTimeout(timeoutId);
	if (options?.signal) {
		options.signal.removeEventListener("abort", killProcess);
	}

	return { stdout, stderr, exitCode: exitCode ?? 0 };
}

export async function git(args: string[], options?: ExecOptions): Promise<ExecResult> {
	const gitPath = Bun.which("git");
	if (!gitPath) {
		return {
			stdout: "",
			stderr: "git not found",
			exitCode: 127,
			error: new GitError("git is not installed", GitErrorCode.UNKNOWN),
		};
	}
	const result = await exec(gitPath, args, options);
	return { ...result, error: detectGitError(result.stderr, result.exitCode) };
}

export async function gh(args: string[], options?: ExecOptions): Promise<ExecResult> {
	const ghPath = Bun.which("gh");
	if (!ghPath) {
		return {
			stdout: "",
			stderr: "gh not found",
			exitCode: 127,
			error: new GitError("GitHub CLI is not installed", GitErrorCode.GH_NOT_INSTALLED),
		};
	}
	const result = await exec(ghPath, args, options);
	return { ...result, error: detectGhError(result.stderr, result.exitCode) };
}

export function parseShortstat(text: string): { files: number; additions: number; deletions: number } | null {
	const match = text.match(/(\d+) files? changed(?:, (\d+) insertions?\(\+\))?(?:, (\d+) deletions?\(-\))?/);
	if (!match) return null;
	return {
		files: Number.parseInt(match[1], 10),
		additions: match[2] ? Number.parseInt(match[2], 10) : 0,
		deletions: match[3] ? Number.parseInt(match[3], 10) : 0,
	};
}

export function isTruthy(value: string | undefined): boolean {
	if (!value) return false;
	return value === "1" || value.toLowerCase() === "true" || value.toLowerCase() === "yes";
}
