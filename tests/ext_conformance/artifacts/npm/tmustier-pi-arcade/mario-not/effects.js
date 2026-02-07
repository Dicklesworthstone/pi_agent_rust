// @ts-check
"use strict";

/**
 * @typedef {Object} Particle
 * @property {number} x
 * @property {number} y
 * @property {number} vx
 * @property {number} vy
 * @property {number} life
 */

/**
 * @typedef {Object} Cue
 * @property {string} text
 * @property {number} ttl
 * @property {boolean} persist
 */

/**
 * @typedef {Object} Config
 * @property {number} gravity
 */

/**
 * @typedef {Object} GameState
 * @property {Config} config
 * @property {Particle[]} particles
 * @property {Cue | null} cue
 */

const PARTICLE_VELOCITIES = [
	{ vx: -0.5, vy: -1.0 },
	{ vx: 0.5, vy: -1.0 },
	{ vx: -0.3, vy: -0.6 },
	{ vx: 0.3, vy: -0.6 },
];
const PARTICLE_LIFE = 0.35;

/** @param {GameState} state @param {number} x @param {number} y @param {number} count */
function spawnParticles(state, x, y, count) {
	for (let i = 0; i < count; i += 1) {
		const vel = PARTICLE_VELOCITIES[i % PARTICLE_VELOCITIES.length];
		state.particles.push({
			x,
			y,
			vx: vel.vx,
			vy: vel.vy,
			life: PARTICLE_LIFE,
		});
	}
}

/** @param {GameState} state @param {number} dt */
function updateParticles(state, dt) {
	const gravity = state.config.gravity * 0.2;
	for (let i = state.particles.length - 1; i >= 0; i -= 1) {
		const p = state.particles[i];
		p.vy += gravity * dt;
		p.x += p.vx * dt;
		p.y += p.vy * dt;
		p.life -= dt;
		if (p.life <= 0) state.particles.splice(i, 1);
	}
}

/** @param {GameState} state @param {string} text @param {number} ttl @param {boolean} persist */
function setCue(state, text, ttl, persist) {
	state.cue = { text, ttl, persist };
}

/** @param {GameState} state @param {number} dt */
function updateCue(state, dt) {
	if (!state.cue || state.cue.persist) return;
	state.cue.ttl -= dt;
	if (state.cue.ttl <= 0) state.cue = null;
}

module.exports = {
	spawnParticles,
	updateParticles,
	setCue,
	updateCue,
};
