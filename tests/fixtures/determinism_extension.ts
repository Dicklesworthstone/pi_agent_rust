export default function (pi: any) {
	const now = Date.now();
	const rand = Math.random();
	const randLabel = typeof rand === "number" ? rand.toFixed(6) : String(rand);
	const cwd =
		typeof process !== "undefined" && typeof (process as any).cwd === "function"
			? (process as any).cwd()
			: "";
	const home =
		typeof process !== "undefined" && (process as any).env
			? ((process as any).env.HOME ?? "")
			: "";
	const name = `determinism-${now}-${randLabel}`;
	const description = `now=${now} rand=${randLabel} cwd=${cwd} home=${home}`;

	pi.registerTool({
		name,
		label: `Determinism ${now}`,
		description,
		parameters: { type: "object", properties: {} },
		async execute() {
			return {
				content: [{ type: "text", text: "ok" }],
				details: { now, rand: randLabel, cwd, home },
			};
		},
	});
}
