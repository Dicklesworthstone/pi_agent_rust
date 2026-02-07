// @ts-check
"use strict";

/** @typedef {import("./types").Level} Level */

/** @param {number} seed @returns {() => number} */
function createRng(seed) {
	let t = seed >>> 0;
	return function next() {
		t += 0x6D2B79F5;
		let r = t;
		r = Math.imul(r ^ (r >>> 15), r | 1);
		r ^= r + Math.imul(r ^ (r >>> 7), r | 61);
		return ((r ^ (r >>> 14)) >>> 0) / 4294967296;
	};
}

/** @param {string[]} lines @returns {Level} */
function makeLevel(lines) {
	if (!Array.isArray(lines) || lines.length === 0) {
		throw new Error("Level must be a non-empty array of strings.");
	}
	const width = lines[0].length;
	for (const line of lines) {
		if (line.length !== width) {
			throw new Error("All level rows must be the same width.");
		}
	}
	return {
		width,
		height: lines.length,
		tiles: lines.map((line) => line.split("")),
	};
}

module.exports = {
	createRng,
	makeLevel,
};
