// @ts-check
"use strict";

const { getTile, setTile, isHazardAt } = require("./tiles.js");
const { spawnParticles, setCue } = require("./effects.js");
const { enterDeath } = require("./death.js");
const { moveHorizontal, resolveVertical, overlaps } = require("./collision.js");
const {
	SCORE_VALUES,
	PLAYER_W,
	PLAYER_H_SMALL,
	PLAYER_H_BIG,
	ITEM_W,
	ITEM_H,
	ITEM_SPEED,
	INVULN_TIME,
	GAME_MODES,
} = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */
/** @typedef {import("./types").InputState} InputState */
/** @typedef {import("./types").PlayerState} PlayerState */

/** @param {PlayerState} player @returns {number} */
function getPlayerHeight(player) {
	return player.size === "big" ? PLAYER_H_BIG : PLAYER_H_SMALL;
}

/** @param {GameState} state @param {InputState} [input] @returns {{ prevY: number, height: number }} */
function stepPlayerMovement(state, input) {
	const cfg = state.config;
	const dt = cfg.dt;
	const player = state.player;
	const prevY = player.y;

	if (player.invuln > 0) {
		player.invuln = Math.max(0, player.invuln - dt);
	}

	const moveLeft = !!(input && input.left);
	const moveRight = !!(input && input.right);
	const jump = !!(input && input.jump);
	const run = !!(input && input.run);
	let move = 0;
	if (moveLeft) move -= 1;
	if (moveRight) move += 1;
	if (move !== 0) player.facing = move;

	const accel = player.onGround ? cfg.groundAccel : cfg.airAccel;
	const maxSpeed = run ? cfg.runSpeed : cfg.walkSpeed;
	if (move !== 0) {
		const target = move * maxSpeed;
		const delta = accel * dt;
		if (player.vx < target) player.vx = Math.min(player.vx + delta, target);
		else if (player.vx > target) player.vx = Math.max(player.vx - delta, target);
	} else if (player.onGround) {
		const delta = cfg.groundDecel * dt;
		if (Math.abs(player.vx) <= delta) player.vx = 0;
		else player.vx -= Math.sign(player.vx) * delta;
	}

	if (jump && player.onGround) {
		player.vy = -cfg.jumpVel;
		player.onGround = false;
	}

	player.vy = Math.min(player.vy + cfg.gravity * dt, cfg.maxFall);

	const height = getPlayerHeight(player);
	const blocked = moveHorizontal(state.level, player, dt, PLAYER_W, height);
	if (blocked) player.vx = 0;

	/** @param {number} tileX @param {number} tileY */
	const onHeadBump = (tileX, tileY) => {
		handleHeadBump(state, tileX, tileY);
	};
	resolveVertical(state.level, player, dt, onHeadBump, height, PLAYER_W, true);

	return { prevY, height };
}

/** @param {GameState} state */
function collectCoin(state) {
	const tileX = Math.floor(state.player.x);
	const tileY = Math.floor(state.player.y);
	if (getTile(state.level, tileX, tileY) === "o") {
		setTile(state.level, tileX, tileY, " ");
		awardCoin(state);
	}
}

/** @param {GameState} state */
function collectItems(state) {
	const player = state.player;
	const height = getPlayerHeight(player);

	state.items = state.items.filter((item) => {
		if (!item.alive) return false;
		if (!overlaps(player.x, player.y, PLAYER_W, height, item.x, item.y, ITEM_W, ITEM_H)) {
			return true; // Keep item
		}
		// Collected - apply effect
		if (player.size === "small") {
			player.size = /** @type {"small" | "big"} */ ("big");
			player.invuln = INVULN_TIME;
			setCue(state, "POWER UP", 0.6, false);
		} else {
			awardScore(state, SCORE_VALUES.mushroom);
		}
		spawnParticles(state, player.x, player.y - 0.2, 4);
		return false; // Remove item
	});
}

/** @param {GameState} state @param {number} tileX @param {number} tileY */
function handleHeadBump(state, tileX, tileY) {
	const tile = getTile(state.level, tileX, tileY);
	if (tile === "?") {
		setTile(state.level, tileX, tileY, "U");
		spawnMushroom(state, tileX, tileY);
	}
}

/** @param {GameState} state @param {number} prevY */
function resolveEnemyCollisions(state, prevY) {
	const player = state.player;
	const height = getPlayerHeight(player);
	const prevBottom = prevY + height;
	const currBottom = player.y + height;
	const falling = currBottom > prevBottom + 0.0001;
	for (const enemy of state.enemies) {
		if (!enemy.alive) continue;
		if (overlaps(player.x, player.y, PLAYER_W, height, enemy.x, enemy.y, 1, 1)) {
			const stomp = falling && prevBottom <= enemy.y + 0.01 && currBottom >= enemy.y;
			if (stomp) {
				enemy.alive = false;
				player.vy = -state.config.jumpVel * 0.6;
				awardScore(state, SCORE_VALUES.stomp);
				spawnParticles(state, enemy.x, enemy.y - 0.2, 4);
			} else {
				applyPlayerDamage(state);
			}
		}
	}
}

/** @param {GameState} state @param {boolean} [forceDeath] */
function applyPlayerDamage(state, forceDeath) {
	const player = state.player;
	if (state.mode !== GAME_MODES.playing) return;
	if (player.invuln > 0) return;
	if (!forceDeath && player.size === "big") {
		player.size = /** @type {"small" | "big"} */ ("small");
		player.invuln = INVULN_TIME;
		return;
	}
	enterDeath(state);
}

/** @param {GameState} state @param {number} height */
function checkGoal(state, height) {
	if (state.mode !== GAME_MODES.playing) return;
	const player = state.player;
	const leftX = player.x + 0.001;
	const rightX = player.x + PLAYER_W - 0.001;
	const topY = player.y - (height - 1);
	/** @param {string} tile */
	const isGoal = (tile) => tile === "G" || tile === "F";

	// Find which position touched the goal
	let touchX = -1;
	let touchY = -1;
	if (isGoal(getTile(state.level, leftX, player.y))) { touchX = Math.floor(leftX); touchY = Math.floor(player.y); }
	else if (isGoal(getTile(state.level, rightX, player.y))) { touchX = Math.floor(rightX); touchY = Math.floor(player.y); }
	else if (isGoal(getTile(state.level, leftX, topY))) { touchX = Math.floor(leftX); touchY = Math.floor(topY); }
	else if (isGoal(getTile(state.level, rightX, topY))) { touchX = Math.floor(rightX); touchY = Math.floor(topY); }

	if (touchX < 0) return;

	// Find top and bottom of flagpole
	let poleTop = touchY;
	let poleBottom = touchY;
	while (poleTop > 0 && isGoal(getTile(state.level, touchX, poleTop - 1))) poleTop--;
	while (poleBottom < state.level.height - 1 && isGoal(getTile(state.level, touchX, poleBottom + 1))) poleBottom++;

	// Calculate flagpole score: higher = more points (100-5000)
	const poleHeight = poleBottom - poleTop + 1;
	const touchHeight = poleBottom - touchY; // 0 at bottom, poleHeight-1 at top
	const heightRatio = poleHeight > 1 ? touchHeight / (poleHeight - 1) : 1;
	const flagScore = Math.floor(100 + heightRatio * 4900);

	// Time bonus: 10 points per second remaining
	const timeBonus = Math.floor(state.time) * 10;

	const totalBonus = flagScore + timeBonus;
	state.score += totalBonus;

	player.vx = 0;
	player.vy = 0;
	setCue(state, `+${totalBonus}`, 0, true);
	state.mode = GAME_MODES.levelClear;
}

/** @param {GameState} state @returns {boolean} */
function checkHazard(state) {
	return isHazardAt(state.level, state.player.x, state.player.y);
}

/** @param {GameState} state @param {number} tileX @param {number} tileY */
function spawnMushroom(state, tileX, tileY) {
	const spawnX = tileX;
	const spawnY = tileY - 1;
	state.items.push({
		x: spawnX,
		y: spawnY,
		vx: ITEM_SPEED,
		vy: 0,
		alive: true,
		onGround: false,
	});
}

/** @param {GameState} state */
function awardCoin(state) {
	state.coins += 1;
	state.score += SCORE_VALUES.coin;
	spawnParticles(state, state.player.x, state.player.y - 0.2, 4);
}

/** @param {GameState} state @param {number} value */
function awardScore(state, value) {
	state.score += value;
}

module.exports = {
	applyPlayerDamage,
	checkGoal,
	checkHazard,
	collectCoin,
	collectItems,
	getPlayerHeight,
	resolveEnemyCollisions,
	stepPlayerMovement,
};
