import type {
	PRActionResult,
	PRCreateResult,
	PRInfo,
	PRListResult,
	PRParams,
	PRResult,
	ToolError,
	ToolResult,
} from "../../types";
import { gh } from "../../utils";

function parseChecks(raw: unknown): { passing: number; failing: number; pending: number } | undefined {
	if (!raw) return undefined;
	if (Array.isArray(raw)) {
		let passing = 0;
		let failing = 0;
		let pending = 0;
		for (const check of raw) {
			const status =
				(check as { state?: string; conclusion?: string }).conclusion ?? (check as { state?: string }).state ?? "";
			switch (status) {
				case "SUCCESS":
				case "success":
					passing += 1;
					break;
				case "FAILURE":
				case "failure":
				case "ERROR":
				case "error":
					failing += 1;
					break;
				default:
					pending += 1;
			}
		}
		return { passing, failing, pending };
	}
	return undefined;
}

type GhPr = {
	number: number;
	title: string;
	state: string;
	author?: { login?: string } | string;
	headRefName?: string;
	baseRefName?: string;
	branch?: string;
	base?: string;
	url: string;
	createdAt: string;
	updatedAt: string;
	additions?: number;
	deletions?: number;
	commits?: number;
	reviewDecision?: string | null;
	checks?: unknown;
	statusCheckRollup?: unknown;
};

function mapPrInfo(raw: GhPr): PRInfo {
	const author = typeof raw.author === "string" ? raw.author : (raw.author?.login ?? "");
	return {
		number: raw.number,
		title: raw.title,
		state: raw.state,
		author,
		branch: raw.headRefName ?? raw.branch ?? "",
		base: raw.baseRefName ?? raw.base ?? "",
		url: raw.url,
		createdAt: raw.createdAt,
		updatedAt: raw.updatedAt,
		additions: raw.additions ?? 0,
		deletions: raw.deletions ?? 0,
		commits: raw.commits ?? 0,
		reviewDecision: raw.reviewDecision ?? undefined,
		checks: parseChecks(raw.checks ?? raw.statusCheckRollup),
	};
}

export async function pr(params: PRParams, cwd?: string): Promise<ToolResult<PRResult> | ToolError> {
	if (params.action === "list") {
		const args = [
			"pr",
			"list",
			"--json",
			"number,title,state,author,headRefName,baseRefName,url,createdAt,updatedAt,additions,deletions,commits,reviewDecision,checks",
		];
		if (params.limit) args.push("--limit", String(params.limit));
		if (params.state) args.push("--state", params.state);
		if (params.author) args.push("--author", params.author);
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const raw = JSON.parse(result.stdout) as GhPr[];
		const prs = raw.map((item) => mapPrInfo(item));
		const data: PRListResult = { prs };
		return { data, _rendered: `PRs: ${prs.length}` };
	}

	if (params.action === "view") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		const result = await gh(
			[
				"pr",
				"view",
				String(params.number),
				"--json",
				"number,title,state,author,headRefName,baseRefName,url,createdAt,updatedAt,additions,deletions,commits,reviewDecision,checks",
			],
			{ cwd },
		);
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const raw = JSON.parse(result.stdout) as GhPr;
		const prInfo = mapPrInfo(raw);
		return { data: { pr: prInfo }, _rendered: `PR #${prInfo.number}: ${prInfo.title}` };
	}

	if (params.action === "create") {
		const args = ["pr", "create"];
		if (params.title) args.push("--title", params.title);
		if (params.body) args.push("--body", params.body);
		if (params.base) args.push("--base", params.base);
		if (params.head) args.push("--head", params.head);
		if (params.draft) args.push("--draft");
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}

		let number = 0;
		let url = "";
		const viewArgs = ["pr", "view", "--json", "number,url"];
		if (params.head) viewArgs.push("--head", params.head);
		const viewResult = await gh(viewArgs, { cwd });
		if (!viewResult.error && viewResult.stdout.trim().length > 0) {
			const raw = JSON.parse(viewResult.stdout) as { number: number; url: string };
			number = raw.number;
			url = raw.url;
		} else {
			const match = result.stdout.match(/https?:\/\/\S+/);
			if (match) {
				url = match[0];
				const numMatch = url.match(/pull\/(\d+)/);
				if (numMatch) number = Number.parseInt(numMatch[1], 10);
			}
		}

		const data: PRCreateResult = { number, url };
		return { data, _rendered: url ? `Created PR ${url}` : "Created PR" };
	}

	if (params.action === "diff") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		const result = await gh(["pr", "diff", String(params.number)], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PRActionResult = { status: "success", diff: result.stdout };
		return { data, _rendered: "PR diff" };
	}

	if (params.action === "checkout") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		const result = await gh(["pr", "checkout", String(params.number)], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PRActionResult = { status: "success" };
		return { data, _rendered: `Checked out PR #${params.number}` };
	}

	if (params.action === "merge") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		const args = ["pr", "merge", String(params.number)];
		if (params.merge_method === "merge") args.push("--merge");
		if (params.merge_method === "squash") args.push("--squash");
		if (params.merge_method === "rebase") args.push("--rebase");
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PRActionResult = { status: "success" };
		return { data, _rendered: `Merged PR #${params.number}` };
	}

	if (params.action === "close") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		const result = await gh(["pr", "close", String(params.number)], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PRActionResult = { status: "success" };
		return { data, _rendered: `Closed PR #${params.number}` };
	}

	if (params.action === "ready") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		const result = await gh(["pr", "ready", String(params.number)], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PRActionResult = { status: "success" };
		return { data, _rendered: `Marked PR #${params.number} ready` };
	}

	if (params.action === "review") {
		if (!params.number) {
			return { error: "PR number required" };
		}
		if (!params.review_action) {
			return { error: "review_action required" };
		}
		if (params.review_action === "comment" && !params.review_body) {
			return { error: "review_body required for comment review" };
		}
		const args = ["pr", "review", String(params.number)];
		if (params.review_action === "approve") args.push("--approve");
		if (params.review_action === "request-changes") args.push("--request-changes");
		if (params.review_action === "comment") args.push("--comment");
		if (params.review_body) args.push("--body", params.review_body);
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: PRActionResult = { status: "success" };
		return { data, _rendered: `Reviewed PR #${params.number}` };
	}

	return { error: `Unknown PR action: ${params.action}` };
}
