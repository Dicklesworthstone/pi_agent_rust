// @ts-check
"use strict";

/**
 * @typedef {Object} Level
 * @property {number} width
 */

/**
 * @typedef {Object} PlayerState
 * @property {number} x
 */

/**
 * @typedef {Object} Config
 * @property {number} viewportWidth
 */

/**
 * @typedef {Object} GameState
 * @property {Level} level
 * @property {PlayerState} player
 * @property {Config} config
 * @property {number} cameraX
 */

/** @param {number} value @param {number} min @param {number} max @returns {number} */
function clamp(value, min, max) {
	return Math.max(min, Math.min(max, value));
}

/** @param {GameState} state @param {number} [viewportWidth] @returns {number} */
function getCameraX(state, viewportWidth) {
	const width = typeof viewportWidth === "number" ? viewportWidth : state.config.viewportWidth;
	const maxX = Math.max(0, state.level.width - width);
	return clamp(state.cameraX || 0, 0, maxX);
}

/** @param {GameState} state @returns {number} */
function updateCamera(state) {
	const width = state.config.viewportWidth;
	const maxX = Math.max(0, state.level.width - width);
	let cameraX = state.cameraX || 0;
	const lead = Math.floor(width * 0.25);
	const minX = cameraX + lead;
	const maxXZone = cameraX + lead;
	if (state.player.x < minX) {
		cameraX = clamp(state.player.x - lead, 0, maxX);
	} else if (state.player.x > maxXZone) {
		cameraX = clamp(state.player.x - lead, 0, maxX);
	}
	state.cameraX = cameraX;
	return cameraX;
}

module.exports = {
	getCameraX,
	updateCamera,
};
