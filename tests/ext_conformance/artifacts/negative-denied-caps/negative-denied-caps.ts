/**
 * Negative conformance test extension: attempts capabilities that should be
 * denied by the default policy (exec and env are in deny_caps).
 *
 * Each tool isolates a single denied operation so tests can verify the exact
 * error message format.
 */
import type { ExtensionAPI } from "@anthropic/pi-extension";

export default function init(pi: ExtensionAPI) {
	// Tool that attempts pi.exec() — should be denied
	pi.registerTool({
		name: "try-exec",
		description: "Attempts a denied exec hostcall",
		parameters: {},
		execute: async () => {
			try {
				const result = await pi.exec("echo", ["hello"]);
				return `UNEXPECTED_SUCCESS: ${JSON.stringify(result)}`;
			} catch (err: any) {
				return `EXEC_DENIED: ${err.message || String(err)}`;
			}
		},
	});

	// Tool that attempts pi.session() — should be allowed (session is in default_caps)
	pi.registerTool({
		name: "try-session",
		description: "Attempts an allowed session hostcall",
		parameters: {},
		execute: async () => {
			try {
				const name = await pi.getSessionName();
				return `SESSION_OK: ${name ?? "(unnamed)"}`;
			} catch (err: any) {
				return `SESSION_ERROR: ${err.message || String(err)}`;
			}
		},
	});

	// Event hook that attempts exec in its handler
	pi.on("session:start", async (_event: any) => {
		try {
			await pi.exec("ls", ["-la"]);
			return { blocked: false, error: null, exec_result: "unexpected_success" };
		} catch (err: any) {
			return {
				blocked: true,
				error: err.message || String(err),
			};
		}
	});
}
