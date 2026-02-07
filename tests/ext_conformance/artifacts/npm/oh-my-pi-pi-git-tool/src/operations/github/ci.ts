import type {
	CIActionResult,
	CIParams,
	CIResult,
	JobInfo,
	RunInfo,
	RunListResult,
	RunViewResult,
	ToolError,
	ToolResult,
} from "../../types";
import { gh } from "../../utils";

type GhRun = {
	databaseId?: number;
	id?: number;
	displayTitle?: string;
	name?: string;
	status: RunInfo["status"];
	conclusion?: RunInfo["conclusion"];
	headBranch?: string;
	branch?: string;
	headSha?: string;
	sha?: string;
	url: string;
	createdAt: string;
	updatedAt: string;
	jobs?: GhJob[];
};

type GhJob = {
	databaseId?: number;
	id?: number;
	name: string;
	status: string;
	conclusion?: string;
	steps?: Array<{ name: string; status: string; conclusion?: string }>;
};

function mapRunInfo(raw: GhRun): RunInfo {
	return {
		id: raw.databaseId ?? raw.id ?? 0,
		name: raw.displayTitle ?? raw.name ?? "",
		status: raw.status,
		conclusion: raw.conclusion ?? undefined,
		branch: raw.headBranch ?? raw.branch ?? "",
		sha: raw.headSha ?? raw.sha ?? "",
		url: raw.url,
		createdAt: raw.createdAt,
		updatedAt: raw.updatedAt,
	};
}

function mapJobInfo(raw: GhJob): JobInfo {
	return {
		id: raw.databaseId ?? raw.id ?? 0,
		name: raw.name,
		status: raw.status,
		conclusion: raw.conclusion ?? undefined,
		steps: (raw.steps ?? []).map((step) => ({
			name: step.name,
			status: step.status,
			conclusion: step.conclusion ?? undefined,
		})),
	};
}

export async function ci(params: CIParams, cwd?: string): Promise<ToolResult<CIResult> | ToolError> {
	if (params.action === "list") {
		const args = [
			"run",
			"list",
			"--json",
			"databaseId,displayTitle,status,conclusion,headBranch,headSha,url,createdAt,updatedAt",
		];
		if (params.limit) args.push("--limit", String(params.limit));
		if (params.branch) args.push("--branch", params.branch);
		if (params.workflow) args.push("--workflow", params.workflow);
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const raw = JSON.parse(result.stdout) as GhRun[];
		const runs = raw.map((item) => mapRunInfo(item));
		const data: RunListResult = { runs };
		return { data, _rendered: `Runs: ${runs.length}` };
	}

	if (params.action === "view") {
		if (!params.run_id) {
			return { error: "run_id required" };
		}
		const result = await gh(
			[
				"run",
				"view",
				String(params.run_id),
				"--json",
				"databaseId,name,status,conclusion,headBranch,headSha,url,createdAt,updatedAt,jobs",
			],
			{ cwd },
		);
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const raw = JSON.parse(result.stdout) as GhRun;
		const run = mapRunInfo(raw);
		const jobs: JobInfo[] = (raw.jobs ?? []).map((job) => mapJobInfo(job));
		let logs: string | undefined;
		if (params.logs_failed) {
			const logsResult = await gh(["run", "view", String(params.run_id), "--log-failed"], { cwd });
			if (!logsResult.error) logs = logsResult.stdout;
		}
		const data: RunViewResult = { run, jobs, ...(logs ? { logs } : {}) };
		return { data, _rendered: `Run ${run.id} ${run.status}` };
	}

	if (params.action === "watch") {
		if (!params.run_id) return { error: "run_id required" };
		const result = await gh(["run", "watch", String(params.run_id), "--exit-status"], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: CIActionResult = { status: "success" };
		return { data, _rendered: `Watched run ${params.run_id}` };
	}

	if (params.action === "run") {
		if (!params.workflow) return { error: "workflow required" };
		const args = ["workflow", "run", params.workflow];
		if (params.inputs) {
			for (const [key, value] of Object.entries(params.inputs)) {
				args.push("--field", `${key}=${value}`);
			}
		}
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: CIActionResult = { status: "success" };
		return { data, _rendered: `Triggered workflow ${params.workflow}` };
	}

	if (params.action === "cancel" || params.action === "rerun") {
		if (!params.run_id) return { error: "run_id required" };
		const result = await gh(["run", params.action, String(params.run_id)], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: CIActionResult = { status: "success" };
		return { data, _rendered: `${params.action} run ${params.run_id}` };
	}

	return { error: `Unknown CI action: ${params.action}` };
}
