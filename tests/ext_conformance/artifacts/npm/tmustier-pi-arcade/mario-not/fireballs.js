// @ts-check
"use strict";

const { FIREBALL_SPEED, FIREBALL_WAVE_AMP, FIREBALL_WAVE_FREQ } = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").FireballState} FireballState */

/** @param {GameState} state */
function updateFireballSpawners(state) {
	const dt = state.config.dt;
	const cameraX = state.cameraX || 0;
	const viewportWidth = state.config.viewportWidth || 40;

	for (const spawner of state.fireballSpawners) {
		// Only spawn fireballs if spawner is near the viewport
		const distanceFromCamera = spawner.x - cameraX;
		const isNearViewport = distanceFromCamera > -10 && distanceFromCamera < viewportWidth + 10;

		if (!isNearViewport) {
			spawner.timer = 0; // Reset timer when off-screen
			continue;
		}

		spawner.timer += dt;
		if (spawner.timer >= spawner.interval) {
			spawner.timer = 0;
			// Spawn a fireball
			state.fireballs.push({
				x: spawner.x,
				y: spawner.y,
				vx: FIREBALL_SPEED * spawner.direction,
				vy: 0,
				alive: true,
				pattern: "wave",
				startY: spawner.y,
			});
		}
	}
}

/** @param {GameState} state */
function updateFireballs(state) {
	const dt = state.config.dt;
	const level = state.level;

	for (let i = state.fireballs.length - 1; i >= 0; i--) {
		const fb = state.fireballs[i];
		if (!fb.alive) {
			state.fireballs.splice(i, 1);
			continue;
		}

		// Move fireball
		fb.x += fb.vx * dt;

		// Wave pattern movement
		if (fb.pattern === "wave") {
			const elapsed = Math.abs(fb.x - (fb.startY + fb.vx * 0)); // Use x position for wave
			fb.y = fb.startY + Math.sin(fb.x * FIREBALL_WAVE_FREQ) * FIREBALL_WAVE_AMP * 0.3;
		}

		// Remove if out of bounds
		if (fb.x < -2 || fb.x > level.width + 2 || fb.y < -2 || fb.y > level.height + 2) {
			state.fireballs.splice(i, 1);
		}
	}
}

/** @param {GameState} state @returns {boolean} */
function checkFireballCollision(state) {
	const player = state.player;
	const playerW = 1;
	const playerH = player.size === "big" ? 2 : 1;

	for (const fb of state.fireballs) {
		if (!fb.alive) continue;

		// Simple AABB collision
		const fbW = 0.5;
		const fbH = 0.5;
		if (
			player.x < fb.x + fbW &&
			player.x + playerW > fb.x &&
			player.y < fb.y + fbH &&
			player.y + playerH > fb.y - (playerH - 1)
		) {
			return true;
		}
	}
	return false;
}

module.exports = {
	updateFireballSpawners,
	updateFireballs,
	checkFireballCollision,
};
