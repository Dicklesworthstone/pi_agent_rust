// @ts-check
"use strict";

const { isSolidAt } = require("./tiles.js");

/** @typedef {import("./types").Level} Level */

/** @param {Level} level @param {number} x @param {number} y @param {number} height @returns {boolean} */
function hitsSolid(level, x, y, height) {
	if (isSolidAt(level, x, y)) return true;
	if (height > 1 && isSolidAt(level, x, y - (height - 1))) return true;
	return false;
}

/** @param {Level} level @param {number} x @param {number} y @param {boolean} allowBottomFall @returns {boolean} */
function isSolidBelow(level, x, y, allowBottomFall) {
	if (allowBottomFall && Math.floor(y) >= level.height) return false;
	return isSolidAt(level, x, y);
}

/**
 * @param {Level} level
 * @param {{ x: number, y: number, vx: number, vy: number, onGround: boolean }} entity
 * @param {number} dt
 * @param {(tileX: number, tileY: number) => void} [onHeadBump]
 * @param {number} [height]
 * @param {number} [width]
 * @param {boolean} [allowBottomFall]
 */
function resolveVertical(level, entity, dt, onHeadBump, height, width, allowBottomFall) {
	const entityHeight = typeof height === "number" ? height : 1;
	const entityWidth = typeof width === "number" ? width : 1;
	const nextY = entity.y + entity.vy * dt;
	const leftX = entity.x + 0.001;
	const rightX = entity.x + entityWidth - 0.001;
	if (entity.vy >= 0) {
		const footY = nextY + 1;
		if (isSolidBelow(level, leftX, footY, !!allowBottomFall) || isSolidBelow(level, rightX, footY, !!allowBottomFall)) {
			entity.y = Math.floor(footY) - 1;
			entity.vy = 0;
			entity.onGround = true;
		} else {
			entity.y = nextY;
			entity.onGround = false;
		}
	} else {
		const headY = nextY - (entityHeight - 1);
		if (isSolidAt(level, leftX, headY) || isSolidAt(level, rightX, headY)) {
			const tileY = Math.floor(headY);
			entity.y = tileY + entityHeight;
			entity.vy = 0;
			if (onHeadBump) {
				const leftTileX = Math.floor(leftX);
				const rightTileX = Math.floor(rightX);
				onHeadBump(leftTileX, tileY);
				if (rightTileX !== leftTileX) onHeadBump(rightTileX, tileY);
			}
		} else {
			entity.y = nextY;
		}
	}
}

/**
 * @param {Level} level
 * @param {{ x: number, y: number, vx: number }} entity
 * @param {number} dt
 * @param {number} width
 * @param {number} height
 * @returns {boolean}
 */
function moveHorizontal(level, entity, dt, width, height) {
	if (!entity.vx) return false;
	const nextX = entity.x + entity.vx * dt;
	const probeX = entity.vx > 0 ? nextX + width - 0.001 : nextX + 0.001;
	if (hitsSolid(level, probeX, entity.y, height)) return true;
	entity.x = nextX;
	return false;
}

/** @param {number} ax @param {number} ay @param {number} aw @param {number} ah @param {number} bx @param {number} by @param {number} bw @param {number} bh @returns {boolean} */
function overlaps(ax, ay, aw, ah, bx, by, bw, bh) {
	return ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by;
}

module.exports = {
	hitsSolid,
	resolveVertical,
	moveHorizontal,
	overlaps,
};
