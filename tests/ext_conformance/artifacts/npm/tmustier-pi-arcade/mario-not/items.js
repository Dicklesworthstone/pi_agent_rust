// @ts-check
"use strict";

const { moveHorizontal, resolveVertical } = require("./collision.js");
const { ITEM_W, ITEM_H } = require("./constants.js");

/** @typedef {import("./types").GameState} GameState */

/** @param {GameState} state */
function updateItems(state) {
	const cfg = state.config;
	for (const item of state.items) {
		if (!item.alive) continue;
		item.vy = Math.min(item.vy + cfg.gravity * cfg.dt, cfg.maxFall);
		const blocked = moveHorizontal(state.level, item, cfg.dt, ITEM_W, ITEM_H);
		if (blocked) item.vx = -item.vx;
		resolveVertical(state.level, item, cfg.dt, undefined, ITEM_H, ITEM_W, true);
		if (item.y >= state.level.height + 1) item.alive = false;
	}
}

module.exports = {
	updateItems,
};
