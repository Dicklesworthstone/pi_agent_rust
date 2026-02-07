import { renderTag } from "../render";
import type { TagInfo, TagParams, TagResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

export async function tag(params: TagParams, cwd?: string): Promise<ToolResult<TagResult> | ToolError> {
	const action = params.action ?? "list";

	if (action === "list") {
		const result = await git(
			[
				"for-each-ref",
				"refs/tags",
				"--format=%(refname:short)%x00%(objectname)%x00%(taggername)%x00%(taggerdate:iso-strict)%x00%(subject)%x00%(objecttype)",
			],
			{ cwd },
		);
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const tags: TagInfo[] = [];
		for (const line of result.stdout.split("\n")) {
			if (!line) continue;
			const parts = line.split("\x00");
			if (parts.length < 6) continue;
			const [name, sha, taggerName, taggerDate, subject, objectType] = parts;
			const annotated = objectType === "tag";
			const tagger = taggerName && taggerDate ? { name: taggerName, date: taggerDate } : undefined;
			tags.push({ name, sha, message: subject || undefined, tagger, annotated });
		}
		const data: TagResult = { tags };
		return { data, _rendered: renderTag(data) };
	}

	if (action === "create") {
		if (!params.name) {
			return { error: "Tag name required" };
		}
		const args = ["tag"];
		if (params.force) args.push("-f");
		if (params.sign) args.push("-s");
		if (params.message) {
			args.push("-a", "-m", params.message);
		}
		args.push(params.name);
		if (params.ref) args.push(params.ref);
		const result = await git(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: TagResult = { status: "success" };
		return { data, _rendered: renderTag(data) };
	}

	if (action === "delete") {
		if (!params.name) {
			return { error: "Tag name required" };
		}
		const result = await git(["tag", "-d", params.name], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: TagResult = { status: "success" };
		return { data, _rendered: renderTag(data) };
	}

	if (action === "push") {
		if (!params.name) {
			return { error: "Tag name required" };
		}
		const result = await git(["push", "origin", params.name], { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: TagResult = { status: "success" };
		return { data, _rendered: renderTag(data) };
	}

	return { error: `Unknown tag action: ${action}` };
}
