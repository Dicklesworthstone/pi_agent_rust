import type {
	IssueCreateResult,
	IssueInfo,
	IssueListResult,
	IssueParams,
	IssueResult,
	ToolError,
	ToolResult,
} from "../../types";
import { gh } from "../../utils";

type GhIssue = {
	number: number;
	title: string;
	state: string;
	author?: { login?: string } | string;
	body?: string | null;
	labels?: Array<{ name?: string } | string>;
	assignees?: Array<{ login?: string } | string>;
	url: string;
	createdAt: string;
	comments?: number;
};

function mapIssueInfo(raw: GhIssue): IssueInfo {
	const author = typeof raw.author === "string" ? raw.author : (raw.author?.login ?? "");
	return {
		number: raw.number,
		title: raw.title,
		state: raw.state,
		author,
		body: raw.body ?? "",
		labels: (raw.labels ?? [])
			.map((label) => (typeof label === "string" ? label : (label.name ?? "")))
			.filter(Boolean),
		assignees: (raw.assignees ?? [])
			.map((assignee) => (typeof assignee === "string" ? assignee : (assignee.login ?? "")))
			.filter(Boolean),
		url: raw.url,
		createdAt: raw.createdAt,
		comments: raw.comments ?? 0,
	};
}

export async function issue(params: IssueParams, cwd?: string): Promise<ToolResult<IssueResult> | ToolError> {
	if (params.action === "list") {
		const args = [
			"issue",
			"list",
			"--json",
			"number,title,state,author,body,labels,assignees,url,createdAt,comments",
		];
		if (params.state) args.push("--state", params.state);
		if (params.labels && params.labels.length > 0) args.push("--label", params.labels.join(","));
		if (params.assignee) args.push("--assignee", params.assignee);
		if (params.limit) args.push("--limit", String(params.limit));
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const raw = JSON.parse(result.stdout) as GhIssue[];
		const issues = raw.map((item) => mapIssueInfo(item));
		const data: IssueListResult = { issues };
		return { data, _rendered: `Issues: ${issues.length}` };
	}

	if (params.action === "view") {
		if (!params.number) {
			return { error: "Issue number required" };
		}
		const result = await gh(
			[
				"issue",
				"view",
				String(params.number),
				"--json",
				"number,title,state,author,body,labels,assignees,url,createdAt,comments",
			],
			{ cwd },
		);
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const issueInfo = mapIssueInfo(JSON.parse(result.stdout) as GhIssue);
		return { data: { issue: issueInfo }, _rendered: `Issue #${issueInfo.number}: ${issueInfo.title}` };
	}

	if (params.action === "create") {
		const args = ["issue", "create"];
		if (params.title) args.push("--title", params.title);
		if (params.body) args.push("--body", params.body);
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		let number = 0;
		let url = "";
		const match = result.stdout.match(/https?:\/\/\S+/);
		if (match) {
			url = match[0];
			const numMatch = url.match(/issues\/(\d+)/);
			if (numMatch) number = Number.parseInt(numMatch[1], 10);
		}
		const data: IssueCreateResult = { number, url };
		return { data, _rendered: url ? `Created issue ${url}` : "Created issue" };
	}

	if (params.action === "close" || params.action === "reopen") {
		if (!params.number) {
			return { error: "Issue number required" };
		}
		const result = await gh(["issue", params.action, String(params.number)], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const verb = params.action === "close" ? "Closed" : "Reopened";
		return { data: { status: "success" }, _rendered: `${verb} issue #${params.number}` };
	}

	if (params.action === "comment") {
		if (!params.number || !params.comment_body) {
			return { error: "Issue number and comment_body required" };
		}
		const result = await gh(["issue", "comment", String(params.number), "--body", params.comment_body], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		return { data: { status: "success" }, _rendered: `Commented on issue #${params.number}` };
	}

	return { error: `Unknown issue action: ${params.action}` };
}
