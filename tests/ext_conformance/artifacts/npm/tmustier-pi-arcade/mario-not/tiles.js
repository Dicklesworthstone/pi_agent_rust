// @ts-check
"use strict";

const { COLORS: C } = require("./colors.js");

/**
 * @typedef {Object} Level
 * @property {number} width
 * @property {number} height
 * @property {string[][]} tiles
 */

/** @type {Set<string>} */
const SOLID_TILES = new Set(["#", "B", "?", "U", "T", "P", "C"]);
/** @type {Set<string>} */
const HAZARD_TILES = new Set(["^", "~", "L"]);

/** @type {Record<string, string>} */
const TILE_GLYPHS = {
	"#": `${C.brown}##${C.reset}`,
	"B": `${C.brown}[]${C.reset}`,
	"?": `${C.brightYellow}??${C.reset}`,
	"U": `${C.gray}..${C.reset}`,
	"o": `${C.brightYellow}o ${C.reset}`,
	"T": `${C.green}||${C.reset}`,
	"P": `${C.green}||${C.reset}`,
	"G": `${C.red}|>${C.reset}`,
	"F": `${C.white}||${C.reset}`,
	"^": `${C.gray}/\\${C.reset}`,
	"~": `${C.blue}~~${C.reset}`,
	"L": `${C.orange}}{${C.reset}`,
	"C": `${C.gray}[]${C.reset}`,
	"A": `${C.white}/\\${C.reset}`,
};

/** @param {string} tile @returns {string} */
function tileGlyph(tile) {
	return TILE_GLYPHS[tile] || "  ";
}

/** @param {Level} level @param {number} x @param {number} y @returns {string} */
function getTile(level, x, y) {
	const tx = Math.floor(x);
	const ty = Math.floor(y);
	if (tx < 0 || tx >= level.width || ty < 0 || ty >= level.height) return " ";
	return level.tiles[ty][tx];
}

/** @param {Level} level @param {number} x @param {number} y @param {string} tile */
function setTile(level, x, y, tile) {
	const tx = Math.floor(x);
	const ty = Math.floor(y);
	if (tx < 0 || tx >= level.width || ty < 0 || ty >= level.height) return;
	level.tiles[ty][tx] = tile;
}

/** @param {Level} level @param {number} x @param {number} y @returns {boolean} */
function isSolidAt(level, x, y) {
	const tx = Math.floor(x);
	const ty = Math.floor(y);
	if (tx < 0 || tx >= level.width || ty < 0 || ty >= level.height) return true;
	return SOLID_TILES.has(level.tiles[ty][tx]);
}

/** @param {Level} level @param {number} x @param {number} y @returns {boolean} */
function isHazardAt(level, x, y) {
	return HAZARD_TILES.has(getTile(level, x, y));
}

module.exports = {
	SOLID_TILES,
	HAZARD_TILES,
	TILE_GLYPHS,
	tileGlyph,
	getTile,
	setTile,
	isSolidAt,
	isHazardAt,
};
