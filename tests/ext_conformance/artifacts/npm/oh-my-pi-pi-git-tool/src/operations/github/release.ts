import type { ReleaseInfo, ReleaseListResult, ReleaseParams, ReleaseResult, ToolError, ToolResult } from "../../types";
import { gh } from "../../utils";

type GhRelease = {
	tagName?: string;
	tag?: string;
	name?: string;
	body?: string;
	isDraft?: boolean;
	draft?: boolean;
	isPrerelease?: boolean;
	prerelease?: boolean;
	createdAt: string;
	publishedAt?: string;
	url: string;
	assets?: Array<{ name: string; size: number; downloadCount?: number; download_count?: number }>;
};

function mapReleaseInfo(raw: GhRelease): ReleaseInfo {
	return {
		tag: raw.tagName ?? raw.tag ?? "",
		name: raw.name ?? "",
		body: raw.body ?? "",
		draft: raw.isDraft ?? raw.draft ?? false,
		prerelease: raw.isPrerelease ?? raw.prerelease ?? false,
		createdAt: raw.createdAt,
		publishedAt: raw.publishedAt ?? "",
		url: raw.url,
		assets: (raw.assets ?? []).map((asset) => ({
			name: asset.name,
			size: asset.size,
			downloadCount: asset.downloadCount ?? asset.download_count ?? 0,
		})),
	};
}

export async function release(params: ReleaseParams, cwd?: string): Promise<ToolResult<ReleaseResult> | ToolError> {
	if (params.action === "list") {
		const args = ["release", "list", "--json", "name,tagName,createdAt,publishedAt,isDraft,isPrerelease,url,assets"];
		if (params.limit) args.push("--limit", String(params.limit));
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const raw = JSON.parse(result.stdout) as GhRelease[];
		const releases = raw.map((item) => mapReleaseInfo(item));
		const data: ReleaseListResult = { releases };
		return { data, _rendered: `Releases: ${releases.length}` };
	}

	if (params.action === "view") {
		if (!params.tag) return { error: "tag required" };
		const result = await gh(
			[
				"release",
				"view",
				params.tag,
				"--json",
				"name,tagName,body,createdAt,publishedAt,isDraft,isPrerelease,url,assets",
			],
			{ cwd },
		);
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const info = mapReleaseInfo(JSON.parse(result.stdout) as GhRelease);
		return { data: info, _rendered: `Release ${info.tag}` };
	}

	if (params.action === "create") {
		if (!params.tag) return { error: "tag required" };
		const args = ["release", "create", params.tag];
		if (params.title) args.push("--title", params.title);
		if (params.notes) args.push("--notes", params.notes);
		if (params.generate_notes) args.push("--generate-notes");
		if (params.draft) args.push("--draft");
		if (params.prerelease) args.push("--prerelease");
		if (params.target) args.push("--target", params.target);
		const result = await gh(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const urlMatch = result.stdout.match(/https?:\/\/\S+/);
		const url = urlMatch ? urlMatch[0] : undefined;
		return { data: { status: "success", url }, _rendered: url ? `Created release ${url}` : "Created release" };
	}

	if (params.action === "delete") {
		if (!params.tag) return { error: "tag required" };
		const result = await gh(["release", "delete", params.tag, "--yes"], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		return { data: { status: "success" }, _rendered: `Deleted release ${params.tag}` };
	}

	if (params.action === "upload") {
		if (!params.tag || !params.assets || params.assets.length === 0) {
			return { error: "tag and assets required" };
		}
		const result = await gh(["release", "upload", params.tag, ...params.assets], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		return { data: { status: "success" }, _rendered: `Uploaded assets to ${params.tag}` };
	}

	return { error: `Unknown release action: ${params.action}` };
}
