// @ts-check
"use strict";

const { updateCamera } = require("./camera.js");
const { updateParticles, setCue, updateCue } = require("./effects.js");
const { updateEnemies } = require("./enemies.js");
const { updateItems } = require("./items.js");
const { stepDeath } = require("./death.js");
const { updateFireballSpawners, updateFireballs, checkFireballCollision } = require("./fireballs.js");
const { updateBoss, resolveBossCollision } = require("./boss.js");
const {
	applyPlayerDamage,
	checkGoal,
	checkHazard,
	collectCoin,
	collectItems,
	resolveEnemyCollisions,
	stepPlayerMovement,
} = require("./player.js");
const { GAME_MODES } = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").InputState} InputState */

/** @param {GameState} state @param {InputState} [input] @returns {GameState} */
function stepGame(state, input) {
	const dt = state.config.dt;
	if (state.mode === GAME_MODES.dead) {
		return stepDeath(state, dt);
	}
	if (state.mode !== GAME_MODES.playing) {
		state.tick += 1;
		return state;
	}

	const { prevY, height } = stepPlayerMovement(state, input);
	updateEnemies(state);
	updateItems(state);
	updateFireballSpawners(state);
	updateFireballs(state);
	updateBoss(state);

	resolveEnemyCollisions(state, prevY);

	// Check boss collision
	if (state.boss && state.boss.alive) {
		const bossHurt = resolveBossCollision(state, prevY);
		if (bossHurt) {
			applyPlayerDamage(state);
		}
	}

	// Check fireball collision
	if (checkFireballCollision(state)) {
		applyPlayerDamage(state);
	}

	collectCoin(state);
	collectItems(state);

	// Only check goal if no boss or boss is defeated
	if (!state.boss || !state.boss.alive) {
		checkGoal(state, height);
	}

	if (state.mode === GAME_MODES.playing) {
		// Check hazards
		if (checkHazard(state)) {
			applyPlayerDamage(state);
		} else if (state.player.y >= state.level.height) {
			applyPlayerDamage(state, true);
		}

		// Update world (only if still playing after hazard check)
		if (state.mode === GAME_MODES.playing) {
			updateCamera(state);
			updateParticles(state, dt);
			updateCue(state, dt);
			state.time = Math.max(0, state.time - dt);
		}
	}

	state.tick += 1;
	return state;
}

/** @param {GameState} state @param {boolean} paused */
function setPaused(state, paused) {
	if (state.mode === GAME_MODES.dead || state.mode === GAME_MODES.gameOver || state.mode === GAME_MODES.levelClear) {
		return state;
	}
	state.mode = paused ? GAME_MODES.paused : GAME_MODES.playing;
	if (paused) {
		setCue(state, "PAUSED", 0, true);
	} else if (state.cue && state.cue.persist) {
		state.cue = null;
	}
	return state;
}

module.exports = {
	stepGame,
	setPaused,
};
