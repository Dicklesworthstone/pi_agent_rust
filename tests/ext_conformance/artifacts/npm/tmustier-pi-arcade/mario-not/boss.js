// @ts-check
"use strict";

const { moveHorizontal, resolveVertical, overlaps } = require("./collision.js");
const { spawnParticles, setCue } = require("./effects.js");
const { BOSS_W, BOSS_H, BOSS_INVULN_TIME, BOSS_SCORE, GAME_MODES } = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").BossState} BossState */

/** @param {GameState} state */
function updateBoss(state) {
	const boss = state.boss;
	if (!boss || !boss.alive) return;

	const cfg = state.config;
	const dt = cfg.dt;

	// Update invulnerability
	if (boss.invuln > 0) {
		boss.invuln = Math.max(0, boss.invuln - dt);
	}

	// Apply gravity
	boss.vy = Math.min(boss.vy + cfg.gravity * dt, cfg.maxFall);

	// Move horizontally
	const blocked = moveHorizontal(state.level, boss, dt, BOSS_W, BOSS_H);
	if (blocked) {
		boss.vx = -boss.vx; // Bounce off walls
	}

	// Resolve vertical movement
	resolveVertical(state.level, boss, dt, undefined, BOSS_H, BOSS_W, true);

	// Check if boss fell into pit
	if (boss.y >= state.level.height + 1) {
		boss.alive = false;
		defeatBoss(state);
	}
}

/** @param {GameState} state @param {number} prevY @returns {boolean} - true if player took damage */
function resolveBossCollision(state, prevY) {
	const boss = state.boss;
	if (!boss || !boss.alive) return false;

	const player = state.player;
	const playerH = player.size === "big" ? 2 : 1;
	const prevBottom = prevY + playerH;
	const currBottom = player.y + playerH;
	const falling = currBottom > prevBottom + 0.0001;

	// Check collision with boss (2x2)
	if (overlaps(player.x, player.y, 1, playerH, boss.x, boss.y - 1, BOSS_W, BOSS_H)) {
		const stomp = falling && prevBottom <= boss.y - 1 + 0.1 && currBottom >= boss.y - 1;
		if (stomp && boss.invuln <= 0) {
			// Player stomped on boss
			damageBoss(state);
			player.vy = -state.config.jumpVel * 0.7;
			return false;
		} else if (boss.invuln <= 0) {
			// Player touched boss from side - take damage
			return true;
		}
	}
	return false;
}

/** @param {GameState} state */
function damageBoss(state) {
	const boss = state.boss;
	if (!boss || !boss.alive) return;

	boss.health -= 1;
	boss.invuln = BOSS_INVULN_TIME;
	spawnParticles(state, boss.x + 1, boss.y - 1, 4);

	if (boss.health <= 0) {
		boss.alive = false;
		defeatBoss(state);
	} else {
		// Boss gets faster when damaged
		const speedMult = 1 + (boss.maxHealth - boss.health) * 0.2;
		boss.vx = Math.sign(boss.vx) * 1.5 * speedMult;
		setCue(state, `BOSS ${boss.health}/${boss.maxHealth}`, 0.5, false);
	}
}

/** @param {GameState} state */
function defeatBoss(state) {
	state.score += BOSS_SCORE;
	spawnParticles(state, state.boss?.x || 0, state.boss?.y || 0, 8);
	setCue(state, "BOSS DEFEATED!", 0, true);
	state.mode = GAME_MODES.levelClear;
}

module.exports = {
	updateBoss,
	resolveBossCollision,
	damageBoss,
	defeatBoss,
};
