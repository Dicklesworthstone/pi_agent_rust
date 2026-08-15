import fs from "node:fs";

const filePath = process.argv[2];
if (!filePath) {
	console.error("Usage: node scripts/check-lcov.mjs <lcov.info>");
	process.exit(1);
}

const text = fs.readFileSync(filePath, "utf8");
const totals = { LF: 0, LH: 0, FNF: 0, FNH: 0, BRF: 0, BRH: 0 };

for (const line of text.split(/\r?\n/)) {
	const match = line.match(/^(LF|LH|FNF|FNH|BRF|BRH):(\d+)$/);
	if (!match) {
		continue;
	}
	const [, key, value] = match;
	totals[key] += Number.parseInt(value, 10);
}

const failures = [
	["lines", totals.LH, totals.LF],
	["functions", totals.FNH, totals.FNF],
	["branches", totals.BRH, totals.BRF],
].filter(([, hit, found]) => found !== 0 && hit !== found);

if (failures.length > 0) {
	for (const [name, hit, found] of failures) {
		console.error(`Coverage check failed for ${name}: ${hit}/${found}`);
	}
	process.exit(1);
}

console.log(
	`Coverage OK: lines ${totals.LH}/${totals.LF}, functions ${totals.FNH}/${totals.FNF}, branches ${totals.BRH}/${totals.BRF}`,
);
