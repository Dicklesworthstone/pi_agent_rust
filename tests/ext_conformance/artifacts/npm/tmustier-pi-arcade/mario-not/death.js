// @ts-check
"use strict";

const { isSolidAt } = require("./tiles.js");
const { setCue } = require("./effects.js");
const { INVULN_TIME, DEATH_WAIT, DEATH_JUMP_VEL, GAME_MODES } = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */

/** @param {GameState} state */
function enterDeath(state) {
	if (state.mode === GAME_MODES.dead || state.mode === GAME_MODES.gameOver) return;
	state.mode = GAME_MODES.dead;
	state.lives = Math.max(0, state.lives - 1);
	state.deathTimer = 0;
	state.deathJumped = false;
	state.player.vx = 0;
	state.player.vy = 0;
	state.player.onGround = false;
}

/** @param {GameState} state */
function respawnPlayer(state) {
	const player = state.player;
	player.x = state.spawnX;
	player.y = state.spawnY;
	player.vx = 0;
	player.vy = 0;
	player.facing = 1;
	player.onGround = isSolidAt(state.level, player.x, player.y + 1);
	player.invuln = INVULN_TIME;
	state.deathTimer = 0;
	state.deathJumped = false;
	state.mode = GAME_MODES.playing;
}

/** @param {GameState} state */
function setGameOver(state) {
	state.mode = GAME_MODES.gameOver;
	state.deathTimer = 0;
	state.deathJumped = false;
	state.player.vx = 0;
	state.player.vy = 0;
	setCue(state, "GAME OVER", 0, true);
}

/** @param {GameState} state @param {number} dt @returns {GameState} */
function stepDeath(state, dt) {
	const player = state.player;
	state.deathTimer += dt;
	if (state.deathTimer < DEATH_WAIT) {
		state.tick += 1;
		return state;
	}
	if (!state.deathJumped) {
		state.deathJumped = true;
		player.vy = -DEATH_JUMP_VEL;
	}
	player.vy = Math.min(player.vy + state.config.gravity * dt, state.config.maxFall);
	player.y += player.vy * dt;
	if (player.y > state.level.height + 2) {
		if (state.lives <= 0) {
			setGameOver(state);
		} else {
			respawnPlayer(state);
		}
	}
	state.tick += 1;
	return state;
}

module.exports = {
	enterDeath,
	respawnPlayer,
	setGameOver,
	stepDeath,
};
