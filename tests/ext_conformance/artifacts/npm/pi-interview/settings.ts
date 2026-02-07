import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

export const SETTINGS_PATH = join(homedir(), ".pi", "agent", "settings.json");

export interface InterviewThemeSettings {
	mode?: "auto" | "light" | "dark";
	name?: string;
	lightPath?: string;
	darkPath?: string;
	toggleHotkey?: string;
}

export interface InterviewSettings {
	browser?: string;
	timeout?: number;
	port?: number;
	theme?: InterviewThemeSettings;
	snapshotDir?: string;      // Default: ~/.pi/interview-snapshots/
	autoSaveOnSubmit?: boolean; // Default: true
}

export function loadSettings(): InterviewSettings {
	try {
		const data = JSON.parse(readFileSync(SETTINGS_PATH, "utf-8"));
		return (data.interview as InterviewSettings) ?? {};
	} catch {
		return {};
	}
}
