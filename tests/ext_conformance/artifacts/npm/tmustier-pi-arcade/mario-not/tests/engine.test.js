"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
	createRng,
	makeLevel,
	createGame,
	stepGame,
	getCameraX,
	updateCamera,
	setPaused,
	saveState,
	loadState,
	snapshotState,
} = require("../engine.js");
const { getTile, isHazardAt, isSolidAt, tileGlyph } = require("../tiles.js");
const { LEVEL_1_LINES, LEVEL_1_WIDTH, LEVEL_1_HEIGHT } = require("../levels.js");
const { GAME_MODES } = require("../constants.js");

test("rng deterministic", () => {
	const rngA = createRng(123);
	const rngB = createRng(123);
	for (let i = 0; i < 5; i += 1) {
		assert.equal(rngA(), rngB());
	}
});

test("makeLevel validates rows", () => {
	assert.throws(() => makeLevel([]), /non-empty/);
	assert.throws(() => makeLevel([" ", "  "]), /same width/);
});

test("createGame defaults level index", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	assert.equal(state.levelIndex, 1);
});

test("createGame extracts enemy tiles", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" E  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	assert.equal(state.enemies.length, 1);
	assert.equal(state.level.tiles[2][1], " ");
	assert.equal(state.enemies[0].onGround, true);
});

test("createGame uses provided start coords", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 2,
		startY: 1,
		config: { dt: 1, gravity: 0 },
	});
	assert.equal(state.player.x, 2);
	assert.equal(state.player.y, 1);
});

test("stepGame clamps time at zero", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	state.time = 0.5;
	stepGame(state, {});
	assert.equal(state.time, 0);
});

test("stepGame reduces time by dt", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	state.time = 10;
	stepGame(state, {});
	assert.equal(state.time, 9);
});

test("paused step does not decrement time", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	setPaused(state, true);
	stepGame(state, {});
	assert.equal(state.time, 300);
});

test("createGame sets player onGround", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	assert.equal(state.player.onGround, true);
});

test("loadState sets item onGround", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.items.push({ x: 1, y: 2, vx: 0, vy: 0, alive: true, onGround: false });
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.items[0].onGround, true);
});

test("save/load preserves level index", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
		levelIndex: 4,
	});
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.levelIndex, 4);
});

test("loadState sets enemy onGround", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.enemies.push({ x: 2, y: 2, vx: 0, vy: 0, alive: true, onGround: false });
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.enemies[0].onGround, true);
});

test("save/load preserves player facing", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.facing = -1;
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.player.facing, -1);
});

test("save/load preserves invuln", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.invuln = 1.5;
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.player.invuln, 1.5);
});

test("save/load preserves player size", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.size = "big";
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.player.size, "big");
});

test("stepGame moves right", () => {
	const level = makeLevel([
		"        ",
		"        ",
		"        ",
		"########",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	stepGame(state, { right: true });
	assert.ok(state.player.x > 1);
	const snap = snapshotState(state);
	assert.equal(snap.player.onGround, true);
});

test("pause freezes time and position", () => {
	const level = makeLevel([
		"        ",
		"        ",
		"        ",
		"########",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	setPaused(state, true);
	stepGame(state, { right: true });
	assert.equal(state.player.x, 1);
	assert.equal(state.time, 300);
	assert.equal(state.mode, GAME_MODES.paused);
	assert.equal(state.cue?.text, "PAUSED");
});

test("unpausing clears pause cue", () => {
	const level = makeLevel([
		"        ",
		"        ",
		"        ",
		"########",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	setPaused(state, true);
	assert.equal(state.cue?.text, "PAUSED");
	setPaused(state, false);
	assert.equal(state.cue, null);
});

test("brick tiles block movement", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"  B ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, walkSpeed: 1, runSpeed: 1, groundAccel: 1 },
	});
	state.player.onGround = true;
	stepGame(state, { right: true });
	assert.equal(state.player.x, 1);
});

test("big player blocks at head height", () => {
	const level = makeLevel([
		"    ",
		"  B ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, walkSpeed: 1, runSpeed: 1, groundAccel: 1 },
	});
	state.player.size = "big";
	state.player.onGround = true;
	stepGame(state, { right: true });
	assert.equal(state.player.x, 1);
});

test("question blocks are solid", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"  ? ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, walkSpeed: 1, runSpeed: 1, groundAccel: 1 },
	});
	state.player.onGround = true;
	stepGame(state, { right: true });
	assert.equal(state.player.x, 1);
});

test("coin pickup updates score and clears tile", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" o  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 0.1, gravity: 0 },
	});
	state.player.onGround = true;
	stepGame(state, {});
	assert.equal(state.coins, 1);
	assert.equal(state.score, 100);
	assert.equal(state.level.tiles[2][1], " ");
	assert.ok(state.particles.length > 0);
});

test("question block spawns mushroom and becomes used", () => {
	const level = makeLevel([
		"    ",
		" ?  ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.vy = -1;
	state.player.onGround = false;
	stepGame(state, {});
	assert.equal(state.items.length, 1);
	assert.equal(state.level.tiles[1][1], "U");
});

test("second question block spawns mushroom", () => {
	const level = makeLevel([
		"    ",
		" ? ?",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.vy = -1;
	state.player.onGround = false;
	stepGame(state, {});
	assert.equal(state.items.length, 1);
	state.player.x = 3;
	state.player.vy = -1;
	state.player.onGround = false;
	stepGame(state, {});
	assert.equal(state.items.length, 2);
	assert.equal(state.level.tiles[1][1], "U");
	assert.equal(state.level.tiles[1][3], "U");
});

test("mushroom pickup grows player", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.items.push({ x: 1, y: 2, vx: 0, vy: 0, alive: true, onGround: false });
	state.player.onGround = true;
	stepGame(state, {});
	assert.equal(state.player.size, "big");
	assert.equal(state.items.length, 0);
});

test("mushroom pickup grants score if already big", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.size = "big";
	state.items.push({ x: 1, y: 2, vx: 0, vy: 0, alive: true, onGround: false });
	state.player.onGround = true;
	stepGame(state, {});
	assert.equal(state.score, 1000);
});

test("stomp defeats enemy", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" E  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 1,
		config: { dt: 1, gravity: 0, jumpVel: 10, enemySpeed: 0 },
	});
	state.player.vy = 1;
	state.player.onGround = false;
	stepGame(state, {});
	assert.equal(state.enemies.length, 1);
	assert.equal(state.enemies[0].alive, false);
	assert.ok(state.player.vy < 0);
	assert.equal(state.score, 50);
});

test("side collision kills player", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" E  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 0,
		startY: 2,
		config: {
			dt: 1,
			gravity: 0,
			walkSpeed: 1,
			runSpeed: 1,
			groundAccel: 1,
			enemySpeed: 0,
		},
	});
	state.player.onGround = true;
	stepGame(state, { right: true });
	assert.equal(state.mode, GAME_MODES.dead);
});

test("big player shrinks on enemy hit", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" E  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, enemySpeed: 0 },
	});
	state.player.size = "big";
	state.player.onGround = true;
	state.player.invuln = 0;
	stepGame(state, {});
	assert.equal(state.player.size, "small");
	assert.equal(state.mode, GAME_MODES.playing);
});

test("hazard tiles kill player", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" ^  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	stepGame(state, {});
	assert.equal(state.mode, GAME_MODES.dead);
});

test("falling into pit kills player", () => {
	const level = makeLevel([
		"     ",
		"     ",
		"     ",
		"#   #",
	]);
	const state = createGame({
		level,
		startX: 2,
		startY: 2,
		config: { dt: 0.1, gravity: 20, maxFall: 20 },
	});
	const startLives = state.lives;
	for (let i = 0; i < 40; i += 1) {
		stepGame(state, {});
		if (state.mode === GAME_MODES.dead) break;
	}
	assert.equal(state.mode, GAME_MODES.dead);
	assert.equal(state.lives, startLives - 1);
});

test("death respawns at spawn with life loss", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" ^  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 0.1, gravity: 20, maxFall: 20 },
	});
	state.player.onGround = true;
	const startLives = state.lives;
	stepGame(state, {});
	assert.equal(state.mode, GAME_MODES.dead);
	assert.equal(state.lives, startLives - 1);
	for (let i = 0; i < 40; i += 1) {
		stepGame(state, {});
		if (state.mode !== GAME_MODES.dead) break;
	}
	assert.equal(state.mode, GAME_MODES.playing);
	assert.equal(state.player.x, state.spawnX);
	assert.equal(state.player.y, state.spawnY);
});

test("enemy falls into pit and despawns", () => {
	const level = makeLevel([
		"     ",
		"     ",
		"  E  ",
		"#   #",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 0.1, gravity: 20, maxFall: 20, enemySpeed: 0 },
	});
	const enemy = state.enemies[0];
	assert.equal(enemy.alive, true);
	for (let i = 0; i < 60; i += 1) {
		stepGame(state, {});
		if (!enemy.alive) break;
	}
	assert.equal(enemy.alive, false);
});

test("goal tile clears level with score bonus", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"  G ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, walkSpeed: 1, runSpeed: 1, groundAccel: 1 },
	});
	state.player.onGround = true;
	const scoreBefore = state.score;
	stepGame(state, { right: true });
	assert.equal(state.mode, GAME_MODES.levelClear);
	assert.ok(state.cue?.text.startsWith("+"), "cue should show score bonus");
	assert.ok(state.score > scoreBefore, "score should increase");
});

test("save and load restores progress", () => {
	const level = makeLevel([
		"    ",
		" ?  ",
		" o  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	stepGame(state, {});
	state.player.vy = -1;
	state.player.onGround = false;
	stepGame(state, {});
	state.enemies.push({ x: 3, y: 2, vx: 0, vy: 0, alive: false, onGround: true });
	state.items.push({ x: 2, y: 2, vx: 0, vy: 0, alive: true, onGround: true });
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.score, state.score);
	assert.equal(loaded.coins, state.coins);
	assert.equal(loaded.level.tiles[2][1], " ");
	assert.equal(loaded.level.tiles[1][1], "U");
	assert.equal(loaded.enemies[0].alive, false);
	assert.equal(loaded.items.length, 2);
});

test("loadState rejects invalid data", () => {
	const invalidVersion = loadState({ version: 2 }, { config: { dt: 1, gravity: 0 } });
	assert.equal(invalidVersion, null);
	const missingLevel = loadState({ version: 1, level: {} }, { config: { dt: 1, gravity: 0 } });
	assert.equal(missingLevel, null);
});

test("save/load preserves mushroom spawn flag", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.mushroomSpawned = true;
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	assert.equal(loaded.mushroomSpawned, true);
});

test("hazard tiles include spikes and water", () => {
	const level = makeLevel([
		" ^~ ",
		"    ",
		"    ",
		"####",
	]);
	assert.equal(isHazardAt(level, 1, 0), true);
	assert.equal(isHazardAt(level, 2, 0), true);
	assert.equal(isHazardAt(level, 0, 0), false);
});

test("solid check treats out-of-bounds as solid", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	assert.equal(isSolidAt(level, -1, 0), true);
	assert.equal(isSolidAt(level, 4, 0), true);
	assert.equal(isSolidAt(level, 0, -1), true);
	assert.equal(isSolidAt(level, 0, 4), true);
	assert.equal(isSolidAt(level, 1, 1), false);
});

test("getTile returns empty for out-of-bounds", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	assert.equal(getTile(level, -1, 0), " ");
	assert.equal(getTile(level, 4, 0), " ");
	assert.equal(getTile(level, 0, -1), " ");
	assert.equal(getTile(level, 0, 4), " ");
	assert.equal(getTile(level, 1, 1), " ");
});

test("tileGlyph defaults to blanks for unknown tiles", () => {
	assert.equal(tileGlyph("!"), "  ");
});

test("saveState includes version", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	const saved = saveState(state);
	assert.equal(saved.version, 1);
	assert.equal(saved.level.lines.length, 4);
});

test("snapshotState rounds numeric fields", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.x = 1.23456;
	state.player.y = 2.34567;
	state.player.vx = 0.123456;
	state.player.vy = -0.987654;
	state.player.invuln = 1.23456;
	state.time = 12.34567;
	const snap = snapshotState(state);
	assert.equal(snap.player.x, 1.235);
	assert.equal(snap.player.y, 2.346);
	assert.equal(snap.player.vx, 0.123);
	assert.equal(snap.player.vy, -0.988);
	assert.equal(snap.player.invuln, 1.235);
	assert.equal(snap.time, 12.346);
});

test("camera clamps within bounds", () => {
	const level = makeLevel([
		"            ",
		"            ",
		"            ",
		"############",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, viewportWidth: 8 },
	});
	state.player.x = 1;
	updateCamera(state);
	assert.equal(getCameraX(state, 8), 0);
	state.player.x = 10;
	updateCamera(state);
	assert.equal(getCameraX(state, 8), 4);
});

test("getCameraX honors override width", () => {
	const level = makeLevel([
		"        ",
		"        ",
		"        ",
		"########",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, viewportWidth: 6 },
	});
	state.cameraX = 3;
	assert.equal(getCameraX(state, 4), 3);
});

test("camera dead-zone holds until edge", () => {
	const level = makeLevel([
		"                    ",
		"                    ",
		"                    ",
		"####################",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0, viewportWidth: 10 },
	});
	state.player.x = 5;
	updateCamera(state);
	assert.equal(state.cameraX, 3);
	state.player.x = 8;
	updateCamera(state);
	assert.equal(state.cameraX, 6);
});

test("level1 dimensions match spec", () => {
	assert.equal(LEVEL_1_LINES.length, LEVEL_1_HEIGHT);
	assert.equal(LEVEL_1_LINES[0].length, LEVEL_1_WIDTH);
});
