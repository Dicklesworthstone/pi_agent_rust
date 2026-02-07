import type { SafetyPolicy } from "../types";

export const defaultPolicy: SafetyPolicy = {
	forcePush: "confirm",
	forcePushMain: "block",
	hardReset: "confirm",
	discardChanges: "warn",
	deleteBranch: "warn",
	amendPushed: "block",
	rebasePushed: "block",
};

export const defaultProtectedBranches = ["main", "master", "develop", "release/*"];

export function isProtectedBranch(branch: string): boolean {
	return defaultProtectedBranches.some((pattern) => {
		if (pattern.endsWith("/")) {
			return branch.startsWith(pattern);
		}
		if (pattern.endsWith("/*")) {
			return branch.startsWith(pattern.slice(0, -1));
		}
		return branch === pattern;
	});
}
