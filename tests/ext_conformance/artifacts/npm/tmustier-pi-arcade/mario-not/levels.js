// @ts-check
"use strict";

const LEVEL_1_WIDTH = 160;
const LEVEL_1_HEIGHT = 15;

/** @param {number} width @param {number} height @param {string} fill @returns {string[][]} */
function makeGrid(width, height, fill) {
	return Array.from({ length: height }, () => Array(width).fill(fill));
}

/** @param {string[][]} grid @param {number} x @param {number} y @param {string} tile */
function setTile(grid, x, y, tile) {
	if (y < 0 || y >= grid.length) return;
	if (x < 0 || x >= grid[0].length) return;
	grid[y][x] = tile;
}

/** @param {string[][]} grid @param {number} y @param {number} x0 @param {number} x1 @param {string} tile */
function fillRow(grid, y, x0, x1, tile) {
	for (let x = x0; x <= x1; x += 1) {
		setTile(grid, x, y, tile);
	}
}

/** @param {string[][]} grid @param {number} x @param {number} baseY @param {number} height */
function makeStaircase(grid, x, baseY, height) {
	for (let step = 0; step < height; step++) {
		for (let row = 0; row <= step; row++) {
			setTile(grid, x + step, baseY - row, "B");
		}
	}
}

/** @param {string[][]} grid @param {number} x @param {number} topY @param {number} baseY */
function makeFlagpole(grid, x, topY, baseY) {
	setTile(grid, x, topY, "G");
	for (let y = topY + 1; y <= baseY; y++) {
		setTile(grid, x, y, "F");
	}
}

/** @param {string[][]} grid @param {number} x @param {number} baseY */
function makePipe(grid, x, baseY) {
	setTile(grid, x, baseY - 1, "T");
	setTile(grid, x + 1, baseY - 1, "T");
	setTile(grid, x, baseY, "P");
	setTile(grid, x + 1, baseY, "P");
}

/** @returns {string[]} */
function buildLevel1() {
	const grid = makeGrid(LEVEL_1_WIDTH, LEVEL_1_HEIGHT, " ");

	// Ground baseline.
	fillRow(grid, 14, 0, LEVEL_1_WIDTH - 1, "#");

	// Segment 1: flat ground, 3-coin line, low brick step.
	setTile(grid, 6, 10, "o");
	setTile(grid, 7, 10, "o");
	setTile(grid, 8, 10, "o");
	setTile(grid, 20, 13, "B");
	setTile(grid, 21, 13, "B");
	setTile(grid, 22, 12, "B");

	// Segment 2: 4-tile gap, single goomba, question block.
	setTile(grid, 50, 14, " ");
	setTile(grid, 51, 14, " ");
	setTile(grid, 52, 14, " ");
	setTile(grid, 53, 14, " ");
	setTile(grid, 60, 13, "E");
	setTile(grid, 70, 11, "?");

	// Segment 3: pipe, two goombas, brick stack.
	makePipe(grid, 90, 14);
	setTile(grid, 100, 13, "E");
	setTile(grid, 106, 13, "E");
	setTile(grid, 110, 13, "B");
	setTile(grid, 110, 12, "B");
	setTile(grid, 112, 13, "B");

	// Segment 4: 2-tile gap, staircase, goal.
	setTile(grid, 130, 14, " ");
	setTile(grid, 131, 14, " ");
	makeStaircase(grid, 140, 13, 4);
	makeFlagpole(grid, 149, 5, 13);

	return grid.map((row) => row.join(""));
}

/** @returns {string[]} */
function buildLevel2() {
	const grid = makeGrid(LEVEL_1_WIDTH, LEVEL_1_HEIGHT, " ");

	// Ground baseline with more gaps.
	fillRow(grid, 14, 0, 45, "#");
	fillRow(grid, 14, 50, 85, "#");
	fillRow(grid, 14, 90, LEVEL_1_WIDTH - 1, "#");

	// Segment 1: elevated platform, coins above.
	setTile(grid, 8, 11, "B");
	setTile(grid, 9, 11, "B");
	setTile(grid, 10, 11, "B");
	setTile(grid, 11, 11, "B");
	setTile(grid, 9, 8, "o");
	setTile(grid, 10, 8, "o");

	// Segment 2: pipe with goomba behind, question block.
	makePipe(grid, 25, 14);
	setTile(grid, 30, 13, "E");
	setTile(grid, 35, 10, "?");

	// Large gap (50-90 is ground, so 46-49 is gap) - need platforms.
	setTile(grid, 47, 12, "B");
	setTile(grid, 48, 12, "B");

	// Segment 3: brick staircase up then down.
	setTile(grid, 55, 13, "B");
	setTile(grid, 56, 13, "B");
	setTile(grid, 56, 12, "B");
	setTile(grid, 57, 13, "B");
	setTile(grid, 57, 12, "B");
	setTile(grid, 57, 11, "B");
	setTile(grid, 58, 13, "B");
	setTile(grid, 58, 12, "B");
	setTile(grid, 59, 13, "B");
	// Coins on top
	setTile(grid, 56, 9, "o");
	setTile(grid, 57, 8, "o");
	setTile(grid, 58, 9, "o");

	// Multiple goombas patrolling.
	setTile(grid, 65, 13, "E");
	setTile(grid, 70, 13, "E");
	setTile(grid, 75, 13, "E");

	// Another question block.
	setTile(grid, 80, 10, "?");

	// Gap at 86-89.
	setTile(grid, 86, 14, " ");
	setTile(grid, 87, 14, " ");
	setTile(grid, 88, 14, " ");
	setTile(grid, 89, 14, " ");

	// Final staircase to flag.
	makeStaircase(grid, 100, 13, 5);
	makeFlagpole(grid, 110, 4, 13);

	return grid.map((row) => row.join(""));
}

/** @returns {string[]} */
function buildLevel3() {
	const grid = makeGrid(LEVEL_1_WIDTH, LEVEL_1_HEIGHT, " ");

	// Sparse ground with many gaps - this is the hard level!
	fillRow(grid, 14, 0, 12, "#");
	fillRow(grid, 14, 18, 30, "#");
	fillRow(grid, 14, 38, 50, "#");
	fillRow(grid, 14, 58, 70, "#");
	fillRow(grid, 14, 80, 95, "#");
	fillRow(grid, 14, 105, 120, "#");
	fillRow(grid, 14, 130, LEVEL_1_WIDTH - 1, "#");

	// Segment 1: spike pit with floating platforms above
	fillRow(grid, 14, 13, 17, "^");  // spikes in first gap
	setTile(grid, 14, 11, "B");  // floating platform
	setTile(grid, 15, 11, "B");
	setTile(grid, 16, 9, "B");  // higher platform
	setTile(grid, 10, 13, "E");  // goomba before gap

	// Coins reward for taking upper path
	setTile(grid, 14, 8, "o");
	setTile(grid, 15, 8, "o");
	setTile(grid, 16, 6, "o");

	// Segment 2: enemy gauntlet
	setTile(grid, 20, 13, "E");
	setTile(grid, 23, 13, "E");
	setTile(grid, 26, 13, "E");
	setTile(grid, 29, 13, "E");
	setTile(grid, 25, 10, "?");  // power-up to help

	// Gap with water (instant death)
	fillRow(grid, 14, 31, 37, "~");
	setTile(grid, 33, 11, "B");  // small platform in middle
	setTile(grid, 34, 11, "B");
	setTile(grid, 33, 8, "o");
	setTile(grid, 34, 8, "o");

	// Segment 3: vertical climb with enemies
	setTile(grid, 40, 13, "B");
	setTile(grid, 40, 12, "B");
	setTile(grid, 42, 11, "B");
	setTile(grid, 42, 10, "B");
	setTile(grid, 44, 9, "B");
	setTile(grid, 44, 8, "B");
	setTile(grid, 46, 7, "B");
	setTile(grid, 47, 7, "B");
	// Coins at the top
	setTile(grid, 46, 4, "o");
	setTile(grid, 47, 4, "o");
	setTile(grid, 48, 13, "E");

	// Spike gap
	fillRow(grid, 14, 51, 57, "^");
	setTile(grid, 53, 10, "B");
	setTile(grid, 54, 10, "B");
	setTile(grid, 55, 10, "B");

	// Segment 4: pipe maze with goombas
	makePipe(grid, 60, 14);
	setTile(grid, 64, 13, "E");
	makePipe(grid, 66, 14);
	setTile(grid, 68, 13, "E");
	setTile(grid, 63, 10, "?");

	// Long water gap - need precise jumping
	fillRow(grid, 14, 71, 79, "~");
	setTile(grid, 73, 11, "B");
	setTile(grid, 76, 12, "B");
	setTile(grid, 77, 12, "B");

	// Segment 5: descending platforms with enemies
	setTile(grid, 82, 10, "B");
	setTile(grid, 83, 10, "B");
	setTile(grid, 84, 10, "E");
	setTile(grid, 86, 11, "B");
	setTile(grid, 87, 11, "B");
	setTile(grid, 89, 12, "B");
	setTile(grid, 90, 12, "B");
	setTile(grid, 91, 13, "E");
	setTile(grid, 93, 13, "E");

	// Another spike gap
	fillRow(grid, 14, 96, 104, "^");
	setTile(grid, 98, 10, "B");
	setTile(grid, 99, 10, "B");
	setTile(grid, 101, 11, "B");
	setTile(grid, 102, 11, "B");

	// Segment 6: final gauntlet
	setTile(grid, 107, 13, "E");
	setTile(grid, 110, 13, "E");
	setTile(grid, 113, 13, "E");
	setTile(grid, 116, 13, "E");
	setTile(grid, 112, 10, "?");  // last power-up

	// Water before final stretch
	fillRow(grid, 14, 121, 129, "~");
	setTile(grid, 123, 11, "B");
	setTile(grid, 124, 11, "B");
	setTile(grid, 126, 10, "B");
	setTile(grid, 127, 10, "B");

	// Final staircase - extra tall
	makeStaircase(grid, 135, 13, 6);
	makeFlagpole(grid, 146, 3, 13);

	return grid.map((row) => row.join(""));
}

/** @returns {string[]} */
function buildLevel4() {
	const grid = makeGrid(LEVEL_1_WIDTH, LEVEL_1_HEIGHT, " ");

	// This is Bowser's Castle - lava floor, fireballs, and a boss fight!

	// Lava baseline - deadly floor throughout most of the level
	fillRow(grid, 14, 0, 20, "L");
	fillRow(grid, 14, 25, 60, "L");
	fillRow(grid, 14, 65, 100, "L");
	fillRow(grid, 14, 105, 130, "L");

	// Safe ground sections
	fillRow(grid, 14, 0, 8, "C");   // Starting platform
	fillRow(grid, 14, 131, LEVEL_1_WIDTH - 1, "C");  // Boss arena

	// Segment 1: Opening gauntlet with floating platforms over lava
	setTile(grid, 10, 12, "C");
	setTile(grid, 11, 12, "C");
	setTile(grid, 14, 10, "C");
	setTile(grid, 15, 10, "C");
	setTile(grid, 18, 11, "C");
	setTile(grid, 19, 11, "C");

	// Coins to guide the way
	setTile(grid, 10, 9, "o");
	setTile(grid, 14, 7, "o");

	// Segment 2: Bridge section with gaps and goombas
	fillRow(grid, 11, 22, 35, "C");  // Bridge
	setTile(grid, 26, 11, " ");  // Gap in bridge
	setTile(grid, 27, 11, " ");
	setTile(grid, 30, 10, "E");  // Goomba on bridge
	setTile(grid, 33, 10, "E");

	// Fireball spawner shooting across the bridge
	setTile(grid, 22, 9, ">");

	// Segment 3: Vertical climb section
	setTile(grid, 38, 12, "C");
	setTile(grid, 39, 12, "C");
	setTile(grid, 41, 10, "C");
	setTile(grid, 42, 10, "C");
	setTile(grid, 44, 8, "C");
	setTile(grid, 45, 8, "C");
	setTile(grid, 47, 6, "C");
	setTile(grid, 48, 6, "C");
	setTile(grid, 50, 8, "C");
	setTile(grid, 51, 8, "C");
	setTile(grid, 53, 10, "C");
	setTile(grid, 54, 10, "C");
	setTile(grid, 56, 12, "C");
	setTile(grid, 57, 12, "C");

	// Power-up for the climb
	setTile(grid, 47, 4, "?");

	// Segment 4: Descending platforms
	fillRow(grid, 10, 60, 64, "C");

	setTile(grid, 67, 11, "C");
	setTile(grid, 68, 11, "C");
	setTile(grid, 71, 12, "C");
	setTile(grid, 72, 12, "C");
	setTile(grid, 75, 11, "C");
	setTile(grid, 76, 11, "C");

	// Goombas on the descent
	setTile(grid, 75, 10, "E");

	// Segment 5: Lava corridor with tight jumps
	setTile(grid, 80, 10, "C");
	setTile(grid, 83, 11, "C");
	setTile(grid, 86, 10, "C");
	setTile(grid, 89, 11, "C");
	setTile(grid, 92, 10, "C");
	setTile(grid, 95, 11, "C");
	setTile(grid, 98, 10, "C");

	// Fireball shooting through the corridor
	setTile(grid, 84, 9, ">");

	// Segment 6: Final approach to boss
	fillRow(grid, 12, 102, 110, "C");
	setTile(grid, 105, 11, "E");
	setTile(grid, 108, 11, "E");

	// Power-up before boss
	setTile(grid, 106, 9, "?");

	// Bridge to boss arena
	fillRow(grid, 12, 112, 130, "C");
	setTile(grid, 118, 12, " ");  // Gap
	setTile(grid, 119, 12, " ");
	setTile(grid, 124, 12, " ");  // Gap
	setTile(grid, 125, 12, " ");

	// Segment 7: Boss Arena
	// Solid floor for boss fight
	fillRow(grid, 13, 131, LEVEL_1_WIDTH - 5, "C");

	// Walls to contain the fight
	for (let y = 8; y <= 13; y++) {
		setTile(grid, 131, y, "C");
	}
	for (let y = 8; y <= 13; y++) {
		setTile(grid, LEVEL_1_WIDTH - 5, y, "C");
	}

	// Bowser!
	setTile(grid, 145, 12, "W");

	// Fireball spawner in arena
	setTile(grid, 150, 10, "<");

	// Coins as a reward for entering
	setTile(grid, 138, 10, "o");
	setTile(grid, 140, 10, "o");
	setTile(grid, 142, 10, "o");

	return grid.map((row) => row.join(""));
}

const LEVEL_1_LINES = buildLevel1();
const LEVEL_2_LINES = buildLevel2();
const LEVEL_3_LINES = buildLevel3();
const LEVEL_4_LINES = buildLevel4();

/** @type {string[][]} */
const ALL_LEVELS = [LEVEL_1_LINES, LEVEL_2_LINES, LEVEL_3_LINES, LEVEL_4_LINES];

module.exports = {
	LEVEL_1_LINES,
	LEVEL_2_LINES,
	LEVEL_3_LINES,
	LEVEL_4_LINES,
	ALL_LEVELS,
	LEVEL_1_WIDTH,
	LEVEL_1_HEIGHT,
};
