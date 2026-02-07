import type { CheckoutParams, CheckoutResult, ToolError, ToolResult } from "../types";
import { git } from "../utils";

export async function checkout(params: CheckoutParams, cwd?: string): Promise<ToolResult<CheckoutResult> | ToolError> {
	const currentResult = await git(["rev-parse", "--abbrev-ref", "HEAD"], { cwd });
	const previous = currentResult.error ? undefined : currentResult.stdout.trim();

	if (params.paths && params.paths.length > 0) {
		const args = ["checkout"];
		if (params.ref) args.push(params.ref);
		if (params.force) args.push("--force");
		args.push("--", ...params.paths);
		const result = await git(args, { cwd });
		if (result.error) {
			return { error: result.error.message, code: result.error.code };
		}
		const data: CheckoutResult = { previous, restoredFiles: params.paths };
		return { data, _rendered: `Restored ${params.paths.length} files` };
	}

	if (!params.ref) {
		return { error: "Ref is required for checkout" };
	}

	const args = ["checkout"];
	if (params.create) args.push("-b");
	if (params.force) args.push("--force");
	args.push(params.ref);

	const result = await git(args, { cwd });
	if (result.error) {
		return { error: result.error.message, code: result.error.code };
	}

	const data: CheckoutResult = { branch: params.ref, previous };
	return { data, _rendered: `Checked out ${params.ref}` };
}
