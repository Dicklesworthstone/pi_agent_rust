// @ts-check
"use strict";

const { createRng, makeLevel } = require("./core.js");
const { DEFAULT_CONFIG, START_LIVES, START_TIME, GAME_MODES } = require("./constants.js");
const { isSolidAt } = require("./tiles.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").SaveState} SaveState */
/** @typedef {import("./types").SnapshotState} SnapshotState */

/** @param {GameState} state @returns {SaveState} */
function saveState(state) {
	return {
		version: 1,
		level: {
			lines: state.level.tiles.map((row) => row.join("")),
		},
		player: {
			x: round(state.player.x),
			y: round(state.player.y),
			vx: round(state.player.vx),
			vy: round(state.player.vy),
			facing: state.player.facing,
			size: state.player.size,
			invuln: round(state.player.invuln),
		},
		enemies: state.enemies.map((enemy) => ({
			x: round(enemy.x),
			y: round(enemy.y),
			vx: round(enemy.vx),
			vy: round(enemy.vy),
			alive: enemy.alive,
		})),
		items: state.items.map((item) => ({
			x: round(item.x),
			y: round(item.y),
			vx: round(item.vx),
			vy: round(item.vy),
			alive: item.alive,
		})),
		fireballs: state.fireballs.map((fb) => ({
			x: round(fb.x),
			y: round(fb.y),
			vx: round(fb.vx),
			vy: round(fb.vy),
			alive: fb.alive,
			pattern: fb.pattern,
			startY: round(fb.startY),
		})),
		fireballSpawners: state.fireballSpawners.map((sp) => ({
			x: sp.x,
			y: sp.y,
			timer: round(sp.timer),
			interval: sp.interval,
			direction: sp.direction,
		})),
		boss: state.boss ? {
			x: round(state.boss.x),
			y: round(state.boss.y),
			vx: round(state.boss.vx),
			vy: round(state.boss.vy),
			alive: state.boss.alive,
			health: state.boss.health,
			maxHealth: state.boss.maxHealth,
			invuln: round(state.boss.invuln),
			onGround: state.boss.onGround,
		} : null,
		score: state.score,
		coins: state.coins,
		lives: state.lives,
		time: round(state.time),
		levelIndex: state.levelIndex,
		mushroomSpawned: state.mushroomSpawned,
		spawnX: round(state.spawnX),
		spawnY: round(state.spawnY),
		mode: state.mode,
	};
}

/**
 * @param {SaveState} save
 * @param {{ config?: Partial<import("./types").Config> }} [options]
 * @returns {GameState | null}
 */
function loadState(save, options) {
	if (!save || save.version !== 1 || !save.level || !Array.isArray(save.level.lines)) {
		return null;
	}
	const config = { ...DEFAULT_CONFIG, ...(options && options.config ? options.config : {}) };
	const level = makeLevel(save.level.lines);
	const spawnX = typeof save.spawnX === "number" ? save.spawnX : save.player.x;
	const spawnY = typeof save.spawnY === "number" ? save.spawnY : save.player.y;
	const mode = normalizeMode(save);
	/** @type {GameState} */
	const state = {
		level,
		rng: createRng(1),
		config,
		tick: 0,
		score: save.score || 0,
		coins: save.coins || 0,
		lives: save.lives || START_LIVES,
		time: typeof save.time === "number" ? save.time : START_TIME,
		levelIndex: save.levelIndex || 1,
		mushroomSpawned: !!save.mushroomSpawned,
		mode,
		cameraX: 0,
		particles: [],
		cue: null,
		spawnX,
		spawnY,
		deathTimer: 0,
		deathJumped: false,
		player: {
			x: save.player.x,
			y: save.player.y,
			vx: save.player.vx,
			vy: save.player.vy,
			facing: save.player.facing,
			onGround: false,
			size: save.player.size,
			invuln: save.player.invuln,
		},
		enemies: save.enemies.map((enemy) => ({
			x: enemy.x,
			y: enemy.y,
			vx: enemy.vx,
			vy: enemy.vy,
			alive: enemy.alive,
			onGround: false,
		})),
		items: save.items.map((item) => ({
			x: item.x,
			y: item.y,
			vx: item.vx,
			vy: item.vy,
			alive: item.alive,
			onGround: false,
		})),
		fireballs: (save.fireballs || []).map((fb) => ({
			x: fb.x,
			y: fb.y,
			vx: fb.vx,
			vy: fb.vy,
			alive: fb.alive,
			pattern: fb.pattern,
			startY: fb.startY,
		})),
		fireballSpawners: (save.fireballSpawners || []).map((sp) => ({
			x: sp.x,
			y: sp.y,
			timer: sp.timer,
			interval: sp.interval,
			direction: sp.direction,
		})),
		boss: save.boss ? {
			x: save.boss.x,
			y: save.boss.y,
			vx: save.boss.vx,
			vy: save.boss.vy,
			alive: save.boss.alive,
			health: save.boss.health,
			maxHealth: save.boss.maxHealth,
			invuln: save.boss.invuln,
			onGround: save.boss.onGround,
		} : null,
	};
	state.player.onGround = isSolidAt(level, state.player.x, state.player.y + 1);
	if (state.mode === GAME_MODES.gameOver) {
		state.cue = { text: "GAME OVER", ttl: 0, persist: true };
	} else if (state.mode === GAME_MODES.paused) {
		state.cue = { text: "PAUSED", ttl: 0, persist: true };
	} else if (state.mode === GAME_MODES.levelClear) {
		state.cue = { text: "LEVEL CLEAR", ttl: 0, persist: true };
	}
	for (const enemy of state.enemies) {
		enemy.onGround = isSolidAt(level, enemy.x, enemy.y + 1);
	}
	for (const item of state.items) {
		item.onGround = isSolidAt(level, item.x, item.y + 1);
	}
	return state;
}

/** @param {GameState} state @returns {SnapshotState} */
function snapshotState(state) {
	return {
		tick: state.tick,
		score: state.score,
		coins: state.coins,
		lives: state.lives,
		time: round(state.time),
		levelIndex: state.levelIndex,
		mushroomSpawned: state.mushroomSpawned,
		mode: state.mode,
		player: {
			x: round(state.player.x),
			y: round(state.player.y),
			vx: round(state.player.vx),
			vy: round(state.player.vy),
			onGround: state.player.onGround,
			facing: state.player.facing,
			size: state.player.size,
			invuln: round(state.player.invuln),
		},
		enemies: state.enemies.map((enemy) => ({
			x: round(enemy.x),
			y: round(enemy.y),
			vx: round(enemy.vx),
			vy: round(enemy.vy),
			alive: enemy.alive,
		})),
		items: state.items.map((item) => ({
			x: round(item.x),
			y: round(item.y),
			vx: round(item.vx),
			vy: round(item.vy),
			alive: item.alive,
			onGround: item.onGround,
		})),
	};
}

/** @param {SaveState} save @returns {"playing" | "paused" | "dead" | "level_clear" | "game_over" | "level_intro"} */
function normalizeMode(save) {
	const mode = save.mode;
	if (mode === GAME_MODES.playing || mode === GAME_MODES.paused || mode === GAME_MODES.dead || mode === GAME_MODES.levelClear || mode === GAME_MODES.gameOver || mode === GAME_MODES.levelIntro) {
		return mode;
	}
	if (save.gameOver) return GAME_MODES.gameOver;
	if (save.paused) return GAME_MODES.paused;
	if (save.player && save.player.dead) return GAME_MODES.dead;
	return GAME_MODES.playing;
}

/** @param {number} value @returns {number} */
function round(value) {
	return Math.round(value * 1000) / 1000;
}

module.exports = {
	saveState,
	loadState,
	snapshotState,
};
