// @ts-check
"use strict";

const { moveHorizontal, resolveVertical } = require("./collision.js");
const { ENEMY_W, ENEMY_H } = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").Level} Level */
/** @typedef {import("./types").EnemyState} EnemyState */

/** @param {GameState} state */
function updateEnemies(state) {
	const cfg = state.config;
	for (const enemy of state.enemies) {
		if (!enemy.alive) continue;
		updateEnemy(state.level, enemy, cfg);
	}
}

/** @param {Level} level @param {EnemyState} enemy @param {import("./types").Config} cfg */
function updateEnemy(level, enemy, cfg) {
	enemy.vy = Math.min(enemy.vy + cfg.gravity * cfg.dt, cfg.maxFall);
	const blocked = moveHorizontal(level, enemy, cfg.dt, ENEMY_W, ENEMY_H);
	if (blocked) enemy.vx = -enemy.vx;
	resolveVertical(level, enemy, cfg.dt, undefined, ENEMY_H, ENEMY_W, true);
	if (enemy.y >= level.height + 1) enemy.alive = false;
}

module.exports = {
	updateEnemies,
};
