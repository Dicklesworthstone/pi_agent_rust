// @ts-check
"use strict";

const { tileGlyph } = require("./tiles.js");
const { getCameraX } = require("./camera.js");
const { COLORS: C, colorize } = require("./colors.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").EnemyState} EnemyState */
/** @typedef {import("./types").ItemState} ItemState */
/** @typedef {import("./types").FireballState} FireballState */
/** @typedef {import("./types").BossState} BossState */

// Animation frames for entities
const PLAYER_FRAMES_SMALL = ["<>", "><"];
const PLAYER_FRAMES_BIG_HEAD = ["<>", "><"];
const PLAYER_FRAMES_BIG_BODY = ["[]"];
const PLAYER_IDLE_SMALL = "<>";
const PLAYER_IDLE_BIG_HEAD = "<>";
const PLAYER_IDLE_BIG_BODY = "[]";
const PLAYER_JUMP_SMALL = "^^";
const PLAYER_JUMP_BIG_HEAD = "^^";
const PLAYER_JUMP_BIG_BODY = "[]";

const ENEMY_FRAMES = [`${C.brown}@@${C.reset}`, `${C.brown}oo${C.reset}`, `${C.brown}@@${C.reset}`, `${C.brown}OO${C.reset}`];
const ITEM_GLYPH = `${C.red}%}${C.reset}`;  // mushroom
const PARTICLE_GLYPH = `${C.brightYellow}**${C.reset}`;
const FIREBALL_FRAMES = [`${C.orange}()${C.reset}`, `${C.brightRed}{}${C.reset}`];
const BOSS_BODY = [`${C.green}MM${C.reset}`, `${C.green}WW${C.reset}`];
const BOSS_HEAD = [`${C.green}><${C.reset}`, `${C.green}<>${C.reset}`];

/** @param {GameState} state @returns {string} */
function getPlayerGlyph(state) {
	const player = state.player;
	const tick = state.tick || 0;
	const isMoving = Math.abs(player.vx) > 0.1;
	const inAir = !player.onGround;
	const color = player.size === "big" ? C.brightCyan : C.cyan;

	// Blink when invulnerable
	if (player.invuln && player.invuln > 0 && Math.floor(tick / 4) % 2 === 0) {
		return "  ";  // invisible during blink
	}

	if (player.size === "big") {
		if (inAir) return colorize(PLAYER_JUMP_BIG_BODY, color);
		if (!isMoving) return colorize(PLAYER_IDLE_BIG_BODY, color);
		const frame = Math.floor(tick / 6) % PLAYER_FRAMES_BIG_BODY.length;
		return colorize(PLAYER_FRAMES_BIG_BODY[frame], color);
	} else {
		if (inAir) return colorize(PLAYER_JUMP_SMALL, color);
		if (!isMoving) return colorize(PLAYER_IDLE_SMALL, color);
		const frame = Math.floor(tick / 6) % PLAYER_FRAMES_SMALL.length;
		return colorize(PLAYER_FRAMES_SMALL[frame], color);
	}
}

/** @param {GameState} state @returns {string} */
function getPlayerHeadGlyph(state) {
	const player = state.player;
	const tick = state.tick || 0;
	const isMoving = Math.abs(player.vx) > 0.1;
	const inAir = !player.onGround;
	const color = C.brightCyan;

	// Blink when invulnerable
	if (player.invuln && player.invuln > 0 && Math.floor(tick / 4) % 2 === 0) {
		return "  ";
	}

	if (inAir) return colorize(PLAYER_JUMP_BIG_HEAD, color);
	if (!isMoving) return colorize(PLAYER_IDLE_BIG_HEAD, color);
	const frame = Math.floor(tick / 6) % PLAYER_FRAMES_BIG_HEAD.length;
	return colorize(PLAYER_FRAMES_BIG_HEAD[frame], color);
}

/** @param {EnemyState} enemy @param {number} tick @returns {string} */
function getEnemyGlyph(enemy, tick) {
	// Animate based on position + tick for variety
	const phase = Math.floor(enemy.x * 3 + tick / 8) % ENEMY_FRAMES.length;
	return ENEMY_FRAMES[phase];
}

/** @param {GameState} state @param {number} viewportWidth @param {number} viewportHeight @returns {string} */
function renderViewport(state, viewportWidth, viewportHeight) {
	const level = state.level;
	const cameraX = getCameraX(state, viewportWidth);
	const rows = [];
	for (let y = 0; y < viewportHeight; y += 1) {
		const row = [];
		for (let x = 0; x < viewportWidth; x += 1) {
			const worldX = Math.floor(cameraX + x);
			const tile =
				worldX >= 0 && worldX < level.width && y >= 0 && y < level.height
					? level.tiles[y][worldX]
					: " ";
			row.push(tileGlyph(tile));
		}
		rows.push(row);
	}
	renderEnemies(rows, state.enemies, cameraX, state.tick);
	renderItems(rows, state.items, cameraX);
	renderParticles(rows, state.particles, cameraX);
	if (state.fireballs) renderFireballs(rows, state.fireballs, cameraX, state.tick);
	if (state.boss) renderBoss(rows, state.boss, cameraX, state.tick);
	const px = Math.floor(state.player.x - cameraX);
	const py = Math.floor(state.player.y);
	const playerGlyph = getPlayerGlyph(state);
	if (py >= 0 && py < viewportHeight && px >= 0 && px < viewportWidth) {
		rows[py][px] = playerGlyph;
		if (state.player.size === "big" && py - 1 >= 0) {
			rows[py - 1][px] = getPlayerHeadGlyph(state);
		}
	}
	const rowStrings = rows.map((row) => row.join(""));
	return applyCue(rowStrings, state.cue).join("\n");
}

/** @param {GameState} state @returns {string} */
function renderFrame(state) {
	const level = state.level;
	const rows = [];
	for (let y = 0; y < level.height; y += 1) {
		const row = [];
		for (let x = 0; x < level.width; x += 1) {
			const tile = level.tiles[y][x];
			row.push(tileGlyph(tile));
		}
		rows.push(row);
	}
	renderEnemies(rows, state.enemies, 0, state.tick);
	renderItems(rows, state.items, 0);
	renderParticles(rows, state.particles, 0);
	const px = Math.floor(state.player.x);
	const py = Math.floor(state.player.y);
	const playerGlyph = getPlayerGlyph(state);
	if (py >= 0 && py < level.height && px >= 0 && px < level.width) {
		rows[py][px] = playerGlyph;
		if (state.player.size === "big" && py - 1 >= 0) {
			rows[py - 1][px] = getPlayerHeadGlyph(state);
		}
	}
	const rowStrings = rows.map((row) => row.join(""));
	return applyCue(rowStrings, state.cue).join("\n");
}

/** @param {GameState} state @param {number} width @returns {string} */
function renderHud(state, width) {
	const time = Math.max(0, Math.ceil(state.time));
	const levelIndex = state.levelIndex || 1;
	const line1 = fitLine(`NOT MARIO L${levelIndex} TIME ${time}`, width);
	const line2 = fitLine(
		`SCORE ${padNum(state.score, 6)} COIN ${padNum(state.coins, 2)} LIVES ${state.lives}`,
		width
	);
	return `${line1}\n${line2}`;
}

/**
 * Set a glyph at position if within bounds
 * @param {string[][]} rows @param {number} x @param {number} y @param {string} glyph
 */
function setGlyph(rows, x, y, glyph) {
	if (y >= 0 && y < rows.length && x >= 0 && x < (rows[0]?.length || 0)) {
		rows[y][x] = glyph;
	}
}

/** @param {string[][]} rows @param {EnemyState[]} enemies @param {number} offsetX @param {number} [tick] */
function renderEnemies(rows, enemies, offsetX, tick) {
	const t = tick || 0;
	for (const enemy of enemies) {
		if (!enemy.alive) continue;
		setGlyph(rows, Math.floor(enemy.x - offsetX), Math.floor(enemy.y), getEnemyGlyph(enemy, t));
	}
}

/** @param {string[][]} rows @param {ItemState[]} items @param {number} offsetX */
function renderItems(rows, items, offsetX) {
	for (const item of items) {
		if (!item.alive) continue;
		setGlyph(rows, Math.floor(item.x - offsetX), Math.floor(item.y), ITEM_GLYPH);
	}
}

/** @param {string[][]} rows @param {{ x: number, y: number, life: number }[]} particles @param {number} offsetX */
function renderParticles(rows, particles, offsetX) {
	for (const p of particles) {
		if (p.life <= 0) continue;
		setGlyph(rows, Math.floor(p.x - offsetX), Math.floor(p.y), PARTICLE_GLYPH);
	}
}

/** @param {string[][]} rows @param {FireballState[]} fireballs @param {number} offsetX @param {number} [tick] */
function renderFireballs(rows, fireballs, offsetX, tick) {
	const t = tick || 0;
	for (const fb of fireballs) {
		if (!fb.alive) continue;
		const frame = Math.floor(t / 4) % FIREBALL_FRAMES.length;
		setGlyph(rows, Math.floor(fb.x - offsetX), Math.floor(fb.y), FIREBALL_FRAMES[frame]);
	}
}

/** @param {string[][]} rows @param {BossState | null} boss @param {number} offsetX @param {number} [tick] */
function renderBoss(rows, boss, offsetX, tick) {
	if (!boss || !boss.alive) return;
	const t = tick || 0;

	// Blink when invulnerable
	if (boss.invuln > 0 && Math.floor(t / 4) % 2 === 0) {
		return;
	}

	const bx = Math.floor(boss.x - offsetX);
	const by = Math.floor(boss.y);
	const frame = Math.floor(t / 8) % BOSS_BODY.length;

	// Render boss (2x2)
	setGlyph(rows, bx, by, BOSS_BODY[frame]);
	setGlyph(rows, bx + 1, by, BOSS_BODY[frame]);
	setGlyph(rows, bx, by - 1, BOSS_HEAD[frame]);
	setGlyph(rows, bx + 1, by - 1, BOSS_HEAD[frame]);
}

/** @param {string[]} rows @param {{ text: string } | null} cue */
function applyCue(rows, cue) {
	if (!cue || !cue.text) return rows;
	const rowIndex = Math.floor(rows.length / 2);
	const row = rows[rowIndex] || "";
	const start = Math.max(0, Math.floor((row.length - cue.text.length) / 2));
	rows[rowIndex] = row.slice(0, start) + cue.text + row.slice(start + cue.text.length);
	return rows;
}

/** @param {string} line @param {number} width @returns {string} */
function fitLine(line, width) {
	if (typeof width !== "number") return line;
	if (line.length >= width) return line.slice(0, width);
	return line.padEnd(width, " ");
}

/** @param {number} value @param {number} length @returns {string} */
function padNum(value, length) {
	return String(value).padStart(length, "0");
}

module.exports = {
	ENEMY_FRAMES,
	ITEM_GLYPH,
	PARTICLE_GLYPH,
	getEnemyGlyph,
	getPlayerGlyph,
	getPlayerHeadGlyph,
	renderFrame,
	renderViewport,
	renderHud,
};
