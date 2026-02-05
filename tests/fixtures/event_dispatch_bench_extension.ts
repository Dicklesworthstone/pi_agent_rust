export default function (pi: any) {
	const noop = async (_event: any, _ctx: any) => {};

	// Event types to benchmark (bd-sas4).
	pi.on("tool_call", noop);
	pi.on("tool_result", noop);
	pi.on("turn_start", noop);
	pi.on("turn_end", noop);
	pi.on("before_agent_start", noop);
	pi.on("input", noop);
	pi.on("context", noop);
	pi.on("resources_discover", noop);
	pi.on("user_bash", noop);
	pi.on("session_before_compact", noop);
	pi.on("session_before_tree", noop);
}

