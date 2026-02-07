import { Type } from "@sinclair/typebox";
import { StringEnum } from "@mariozechner/pi-ai";
import { Text } from "@mariozechner/pi-tui";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import * as path from "node:path";
import * as os from "node:os";
import * as fs from "node:fs";
import { randomUUID } from "node:crypto";
import { execSync } from "node:child_process";
import { startInterviewServer, getActiveSessions, type ResponseItem } from "./server.js";
import { validateQuestions, type QuestionsFile } from "./schema.js";
import { loadSettings, type InterviewThemeSettings } from "./settings.js";

function formatTimeAgo(timestamp: number): string {
	const seconds = Math.floor((Date.now() - timestamp) / 1000);
	if (seconds < 0) return "just now";
	if (seconds < 60) return `${seconds} seconds ago`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return minutes === 1 ? "1 minute ago" : `${minutes} minutes ago`;
	const hours = Math.floor(minutes / 60);
	return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
}

async function openUrl(pi: ExtensionAPI, url: string, browser?: string): Promise<void> {
	const platform = os.platform();
	let result;
	if (platform === "darwin") {
		if (browser) {
			result = await pi.exec("open", ["-a", browser, url]);
		} else {
			result = await pi.exec("open", [url]);
		}
	} else if (platform === "win32") {
		if (browser) {
			result = await pi.exec("cmd", ["/c", "start", "", browser, url]);
		} else {
			result = await pi.exec("cmd", ["/c", "start", "", url]);
		}
	} else {
		if (browser) {
			result = await pi.exec(browser, [url]);
		} else {
			result = await pi.exec("xdg-open", [url]);
		}
	}
	if (result.code !== 0) {
		throw new Error(result.stderr || `Failed to open browser (exit code ${result.code})`);
	}
}

interface InterviewDetails {
	status: "completed" | "cancelled" | "timeout" | "aborted" | "queued";
	responses: ResponseItem[];
	url: string;
	queuedMessage?: string;
}

// Types for saved interviews
interface SavedFromMeta {
	cwd: string;
	branch: string | null;
	sessionId: string;
}

interface SavedQuestionsFile extends QuestionsFile {
	savedAnswers?: ResponseItem[];
	savedAt?: string;
	wasSubmitted?: boolean;
	savedFrom?: SavedFromMeta;
}

const InterviewParams = Type.Object({
	questions: Type.String({ description: "Path to questions JSON or saved interview HTML file" }),
	timeout: Type.Optional(
		Type.Number({ description: "Seconds before auto-timeout", default: 600 })
	),
	verbose: Type.Optional(Type.Boolean({ description: "Enable debug logging", default: false })),
	theme: Type.Optional(
		Type.Object(
			{
				mode: Type.Optional(StringEnum(["auto", "light", "dark"])),
				name: Type.Optional(Type.String()),
				lightPath: Type.Optional(Type.String()),
				darkPath: Type.Optional(Type.String()),
				toggleHotkey: Type.Optional(Type.String()),
			},
			{ additionalProperties: false }
		)
	),
});

function expandHome(value: string): string {
	if (value === "~") {
		return os.homedir();
	}
	// Handle both Unix (/) and Windows (\) separators for user convenience
	if (value.startsWith("~/") || value.startsWith("~\\")) {
		return path.join(os.homedir(), value.slice(2));
	}
	return value;
}

function resolveOptionalPath(value: string | undefined, cwd: string): string | undefined {
	if (!value) return undefined;
	const expanded = expandHome(value);
	return path.isAbsolute(expanded) ? expanded : path.join(cwd, expanded);
}

const DEFAULT_THEME_HOTKEY = "mod+shift+l";

function mergeThemeConfig(
	base: InterviewThemeSettings | undefined,
	override: InterviewThemeSettings | undefined,
	cwd: string
): InterviewThemeSettings {
	const merged: InterviewThemeSettings = { ...(base ?? {}), ...(override ?? {}) };
	return {
		...merged,
		toggleHotkey: merged.toggleHotkey ?? DEFAULT_THEME_HOTKEY,
		lightPath: resolveOptionalPath(merged.lightPath, cwd),
		darkPath: resolveOptionalPath(merged.darkPath, cwd),
	};
}

function loadQuestions(questionsPath: string, cwd: string): SavedQuestionsFile {
	// Expand ~ first, then check if absolute
	const expanded = expandHome(questionsPath);
	const absolutePath = path.isAbsolute(expanded)
		? expanded
		: path.join(cwd, questionsPath); // Use original if relative (no ~)

	if (!fs.existsSync(absolutePath)) {
		throw new Error(`Questions file not found: ${absolutePath}`);
	}

	const content = fs.readFileSync(absolutePath, "utf-8");

	// Handle HTML files (saved interviews)
	if (absolutePath.endsWith(".html") || absolutePath.endsWith(".htm")) {
		return loadSavedInterview(content, absolutePath);
	}

	// Original JSON handling
	let data: unknown;
	try {
		data = JSON.parse(content);
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		throw new Error(`Invalid JSON in questions file: ${message}`);
	}

	return validateQuestions(data);
}

function loadSavedInterview(html: string, filePath: string): SavedQuestionsFile {
	// Extract JSON from <script id="pi-interview-data">
	const match = html.match(/<script[^>]+id=["']pi-interview-data["'][^>]*>([\s\S]*?)<\/script>/i);
	if (!match) {
		throw new Error("Invalid saved interview: missing embedded data");
	}

	let data: unknown;
	try {
		data = JSON.parse(match[1]);
	} catch {
		throw new Error("Invalid saved interview: malformed JSON");
	}

	const raw = data as Record<string, unknown>;
	const validated = validateQuestions(data);

	// Resolve relative image paths to absolute based on HTML file location
	const snapshotDir = path.dirname(filePath);
	const savedAnswers = Array.isArray(raw.savedAnswers)
		? resolveAnswerPaths(raw.savedAnswers as ResponseItem[], snapshotDir)
		: undefined;

	// Validate savedFrom if present
	let savedFrom: SavedFromMeta | undefined;
	if (raw.savedFrom && typeof raw.savedFrom === "object") {
		const sf = raw.savedFrom as Record<string, unknown>;
		if (typeof sf.cwd === "string" && typeof sf.sessionId === "string") {
			savedFrom = {
				cwd: sf.cwd,
				branch: typeof sf.branch === "string" ? sf.branch : null,
				sessionId: sf.sessionId,
			};
		}
	}

	// Return validated questions plus saved interview metadata
	return {
		...validated,
		savedAnswers,
		savedAt: typeof raw.savedAt === "string" ? raw.savedAt : undefined,
		wasSubmitted: typeof raw.wasSubmitted === "boolean" ? raw.wasSubmitted : undefined,
		savedFrom,
	};
}

function resolveAnswerPaths(answers: ResponseItem[], baseDir: string): ResponseItem[] {
	return answers.map((ans) => ({
		...ans,
		value: resolvePathValue(ans.value, baseDir),
		attachments: ans.attachments?.map((p) => resolveImagePath(p, baseDir)),
	}));
}

function resolveImagePath(p: string, baseDir: string): string {
	if (!p) return p;
	// Skip URLs
	if (p.includes("://")) return p;
	// Expand ~ first
	const expanded = expandHome(p);
	// Don't resolve if already absolute (cross-platform check)
	if (path.isAbsolute(expanded)) {
		return expanded;
	}
	// Resolve relative path against snapshot directory
	return path.join(baseDir, p);
}

function resolvePathValue(value: string | string[], baseDir: string): string | string[] {
	if (Array.isArray(value)) {
		return value.map((v) => resolveImagePath(v, baseDir));
	}
	return typeof value === "string" && value ? resolveImagePath(value, baseDir) : value;
}

function formatResponses(responses: ResponseItem[]): string {
	if (responses.length === 0) return "(none)";
	return responses
		.map((resp) => {
			const value = Array.isArray(resp.value) ? resp.value.join(", ") : resp.value;
			let line = `- ${resp.id}: ${value}`;
			if (resp.attachments && resp.attachments.length > 0) {
				line += ` [attachments: ${resp.attachments.join(", ")}]`;
			}
			return line;
		})
		.join("\n");
}

function hasAnyAnswers(responses: ResponseItem[]): boolean {
	if (!responses || responses.length === 0) return false;
	return responses.some((resp) => {
		if (!resp || resp.value == null) return false;
		if (Array.isArray(resp.value)) {
			return resp.value.some((v) => typeof v === "string" && v.trim() !== "");
		}
		return typeof resp.value === "string" && resp.value.trim() !== "";
	});
}

function filterAnsweredResponses(responses: ResponseItem[]): ResponseItem[] {
	if (!responses) return [];
	return responses.filter((resp) => {
		if (!resp || resp.value == null) return false;
		if (Array.isArray(resp.value)) {
			return resp.value.some((v) => typeof v === "string" && v.trim() !== "");
		}
		return typeof resp.value === "string" && resp.value.trim() !== "";
	});
}

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "interview",
		label: "Interview",
		description:
			"Present an interactive form to gather user responses. " +
			"Use proactively when: choosing between multiple approaches, gathering requirements before implementation, " +
			"exploring design tradeoffs, or when decisions have multiple dimensions worth discussing. " +
			"Provides better UX than back-and-forth chat for structured input. " +
			"Image responses and attachments are returned as file paths - use read tool directly to display them. " +
			'Questions JSON format: { "title": "...", "questions": [{ "id": "q1", "type": "single|multi|text|image", "question": "...", "options": ["A", "B"], "codeBlock": { "code": "...", "lang": "ts" } }] }. ' +
			"Options can be strings or objects: { label: string, code?: { code, lang?, file?, lines?, highlights? } }. " +
			"Questions can have a codeBlock field to display code above options. Types: single (radio), multi (checkbox), text (textarea), image (file upload).",
		parameters: InterviewParams,

		async execute(_toolCallId, params, signal, onUpdate, ctx) {
			const { questions, timeout, verbose, theme } = params as {
				questions: string;
				timeout?: number;
				verbose?: boolean;
				theme?: InterviewThemeSettings;
			};

			if (!ctx.hasUI) {
				throw new Error(
					"Interview tool requires interactive mode with browser support. " +
						"Cannot run in headless/RPC/print mode."
				);
			}

			if (typeof ctx.hasQueuedMessages === "function" && ctx.hasQueuedMessages()) {
				return {
					content: [{ type: "text", text: "Interview skipped - user has queued input." }],
					details: { status: "cancelled", url: "", responses: [] },
				};
			}

			const settings = loadSettings();
			const timeoutSeconds = timeout ?? settings.timeout ?? 600;
			const themeConfig = mergeThemeConfig(settings.theme, theme, ctx.cwd);
			const questionsData = loadQuestions(questions, ctx.cwd);

			// Expand ~ in snapshotDir if present
			const snapshotDir = settings.snapshotDir
				? expandHome(settings.snapshotDir)
				: undefined; // Server will use default

			if (signal?.aborted) {
				return {
					content: [{ type: "text", text: "Interview was aborted." }],
					details: { status: "aborted", url: "", responses: [] },
				};
			}

			const sessionId = randomUUID();
			const sessionToken = randomUUID();
			let server: { close: () => void } | null = null;
			let resolved = false;
			let url = "";

			const cleanup = () => {
				if (server) {
					server.close();
					server = null;
				}
			};

			return new Promise((resolve, reject) => {
				const finish = (
					status: InterviewDetails["status"],
					responses: ResponseItem[] = [],
					cancelReason?: "timeout" | "user" | "stale"
				) => {
					if (resolved) return;
					resolved = true;
					cleanup();

					let text = "";
					if (status === "completed") {
						text = `User completed the interview form.\n\nResponses:\n${formatResponses(responses)}`;
					} else if (status === "cancelled") {
						if (cancelReason === "stale") {
							text =
								"Interview session ended due to lost heartbeat.\n\nQuestions saved to: ~/.pi/interview-recovery/";
						} else if (hasAnyAnswers(responses)) {
							const answered = filterAnsweredResponses(responses);
							text = `User cancelled the interview with partial responses:\n${formatResponses(answered)}\n\nProceed with these inputs and use your best judgment for unanswered questions.`;
						} else {
							text = "User skipped the interview without providing answers. Proceed with your best judgment - use recommended options where specified, make reasonable choices elsewhere. Don't ask for clarification unless absolutely necessary.";
						}
					} else if (status === "timeout") {
						if (hasAnyAnswers(responses)) {
							const answered = filterAnsweredResponses(responses);
							text = `Interview form timed out after ${timeoutSeconds} seconds.\n\nPartial responses before timeout:\n${formatResponses(answered)}\n\nQuestions saved to: ~/.pi/interview-recovery/\n\nProceed with these inputs and use your best judgment for unanswered questions.`;
						} else {
							text = `Interview form timed out after ${timeoutSeconds} seconds.\n\nQuestions saved to: ~/.pi/interview-recovery/`;
						}
					} else {
						text = "Interview was aborted.";
					}

					resolve({
						content: [{ type: "text", text }],
						details: { status, url, responses },
					});
				};

				const handleAbort = () => finish("aborted");
				signal?.addEventListener("abort", handleAbort, { once: true });

				startInterviewServer(
					{
						questions: questionsData,
						sessionToken,
						sessionId,
						cwd: ctx.cwd,
						timeout: timeoutSeconds,
						port: settings.port,
						verbose,
						theme: themeConfig,
						snapshotDir,
						autoSaveOnSubmit: settings.autoSaveOnSubmit ?? true,
						savedAnswers: questionsData.savedAnswers,
					},
					{
						onSubmit: (responses) => finish("completed", responses),
						onCancel: (reason, partialResponses) =>
							reason === "timeout"
								? finish("timeout", partialResponses ?? [])
								: finish("cancelled", partialResponses ?? [], reason),
					}
				)
					.then(async (handle) => {
						server = handle;
						url = handle.url;

						const activeSessions = getActiveSessions();
						const otherActive = activeSessions.filter((s) => s.id !== sessionId);

						if (otherActive.length > 0) {
							const active = otherActive[0];
							const queuedLines = [
								"Interview already active in browser:",
								`  Title: ${active.title}`,
								`  Project: ${active.cwd}${active.gitBranch ? ` (${active.gitBranch})` : ""}`,
								`  Session: ${active.id.slice(0, 8)}`,
								`  Started: ${formatTimeAgo(active.startedAt)}`,
								"",
								"New interview ready:",
								`  Title: ${questionsData.title || "Interview"}`,
							];
							const normalizedCwd = ctx.cwd.startsWith(os.homedir())
								? "~" + ctx.cwd.slice(os.homedir().length)
								: ctx.cwd;
							const gitBranch = (() => {
								try {
									return execSync("git rev-parse --abbrev-ref HEAD", {
										cwd: ctx.cwd,
										encoding: "utf8",
										timeout: 2000,
										stdio: ["pipe", "pipe", "pipe"],
									}).trim() || null;
								} catch {
									return null;
								}
							})();
							queuedLines.push(`  Project: ${normalizedCwd}${gitBranch ? ` (${gitBranch})` : ""}`);
							queuedLines.push(`  Session: ${sessionId.slice(0, 8)}`);
							queuedLines.push("");
							queuedLines.push(`Open when ready: ${url}`);
							queuedLines.push("");
							queuedLines.push("Server waiting until you open the link.");
							const queuedMessage = queuedLines.join("\n");
							const queuedSummary = "Interview queued; see tool panel for link.";
							if (onUpdate) {
								onUpdate({
									content: [{ type: "text", text: queuedSummary }],
									details: { status: "queued", url, responses: [], queuedMessage },
								});
							} else if (pi.hasUI) {
								pi.ui.notify(queuedSummary, "info");
							}
						} else {
							try {
								await openUrl(pi, url, settings.browser);
							} catch (err) {
								cleanup();
								const message = err instanceof Error ? err.message : String(err);
								reject(new Error(`Failed to open browser: ${message}`));
								return;
							}
						}
					})
					.catch((err) => {
						cleanup();
						reject(err);
					});
			});
		},

		renderCall(args, theme) {
			const { questions } = args as { questions?: string };
			const label = questions ? `Interview: ${questions}` : "Interview";
			return new Text(theme.fg("toolTitle", theme.bold(label)), 0, 0);
		},

		renderResult(result, _options, theme) {
			const details = result.details as InterviewDetails | undefined;
			if (!details) return new Text("Interview", 0, 0);

			if (details.status === "queued" && details.queuedMessage) {
				const header = theme.fg("warning", "QUEUED");
				const body = theme.fg("dim", details.queuedMessage);
				return new Text(`${header}\n${body}`, 0, 0);
			}

			const statusColor =
				details.status === "completed"
					? "success"
					: details.status === "cancelled"
						? "warning"
						: details.status === "timeout"
							? "warning"
							: details.status === "queued"
								? "warning"
								: "error";

			const line = `${details.status.toUpperCase()} (${details.responses.length} responses)`;
			return new Text(theme.fg(statusColor, line), 0, 0);
		},
	});
}
