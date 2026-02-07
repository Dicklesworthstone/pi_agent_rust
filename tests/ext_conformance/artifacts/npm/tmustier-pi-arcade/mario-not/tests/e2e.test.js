"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const {
	makeLevel,
	createGame,
	stepGame,
	renderFrame,
	renderViewport,
	renderHud,
	saveState,
	loadState,
	setPaused,
} = require("../engine.js");

test("e2e: one step right renders expected frame", () => {
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
	const frame = renderFrame(state);
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story0-frame.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: camera clamps to right edge", () => {
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
	state.player.onGround = true;
	for (let i = 0; i < 3; i += 1) {
		stepGame(state, { right: true });
	}
	const frame = renderViewport(state, 8, 4);
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story1-camera.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: camera uses state offset", () => {
	const level = makeLevel([
		"B?oG##",
		"      ",
		"      ",
		"######",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.cameraX = 2;
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story20-camera-offset.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: camera clamps negative offset", () => {
	const level = makeLevel([
		"B?oG",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.cameraX = -3;
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story23-camera-negative.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: camera clamps positive offset", () => {
	const level = makeLevel([
		"B?oGTP^~",
		"        ",
		"        ",
		"########",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.cameraX = 20;
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story25-camera-positive.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: camera uses viewport width param", () => {
	const level = makeLevel([
		"B?oGTP^~",
		"        ",
		"        ",
		"########",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 6 },
	});
	state.cameraX = 3;
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story24-camera-width.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: tile glyphs render consistently", () => {
	const level = makeLevel([
		" B?oG ",
		"      ",
		"      ",
		"######",
	]);
	const state = createGame({
		level,
		startX: 1,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	const frame = renderFrame(state);
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story1-glyphs.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: unknown tiles render blank", () => {
	const level = makeLevel([
		"!  ",
		"   ",
		"   ",
		"###",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0 },
	});
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story16-unknown-tile.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: enemy and item render together", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" E  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 3,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.items.push({ x: 2, y: 2, vx: 0, vy: 0, alive: true, onGround: true });
	state.player.onGround = true;
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story17-mix.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: enemy renders in viewport", () => {
	const level = makeLevel([
		"      ",
		"      ",
		"   E  ",
		"######",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.cameraX = 1;
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story29-enemy-viewport.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: item renders in viewport", () => {
	const level = makeLevel([
		"      ",
		"      ",
		"      ",
		"######",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.cameraX = 1;
	state.items.push({ x: 3, y: 1, vx: 0, vy: 0, alive: true, onGround: true });
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story28-item-viewport.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: hazard glyphs render", () => {
	const level = makeLevel([
		" ^~ ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 3,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story11-hazards.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: pipe glyphs render", () => {
	const level = makeLevel([
		" T ",
		" P ",
		"   ",
		"###",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0 },
	});
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story13-pipes.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: goal glyph renders", () => {
	const level = makeLevel([
		" G ",
		"   ",
		"   ",
		"###",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0 },
	});
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story14-goal.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: used block glyph renders", () => {
	const level = makeLevel([
		" U ",
		"   ",
		"   ",
		"###",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0 },
	});
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story12-used-block.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: enemy renders with goomba glyph", () => {
	const level = makeLevel([
		"    ",
		"    ",
		" E  ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 3,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	const frame = renderFrame(state);
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story2-enemy.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: hud shows score and coins", () => {
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
		config: { dt: 1, gravity: 0 },
	});
	state.player.onGround = true;
	stepGame(state, {});
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story3-hud.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud rounds time up", () => {
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
	state.time = 12.1;
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story8-hud-time.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud clamps negative time", () => {
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
	state.time = -5;
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story21-hud-zero.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud truncates to width", () => {
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
	const hud = renderHud(state, 10)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story15-hud-narrow.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud shows level index", () => {
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
		levelIndex: 3,
	});
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story9-hud-level.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud pads score and coins", () => {
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
		levelIndex: 2,
	});
	state.score = 42;
	state.coins = 3;
	state.lives = 5;
	state.time = 123;
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story18-hud-score.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud shows lives", () => {
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
	state.lives = 7;
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story26-hud-lives.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud shows score", () => {
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
	state.score = 7;
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story30-hud-score.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: hud shows coins", () => {
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
	state.coins = 9;
	const hud = renderHud(state, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story27-hud-coins.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: cue overlays frame", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0 },
	});
	state.cue = { text: "READY", ttl: 1, persist: false };
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story19-cue.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: item renders with mushroom glyph", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 3,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.items.push({ x: 1, y: 2, vx: 0, vy: 0, alive: true, onGround: true });
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story10-item.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: big player renders tall", () => {
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
	state.player.onGround = true;
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story4-big.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: big player renders in viewport", () => {
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
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.player.size = "big";
	state.player.onGround = true;
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story22-big-viewport.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: save + load preserves hud", () => {
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
	const saved = saveState(state);
	const loaded = loadState(saved, { config: { dt: 1, gravity: 0 } });
	assert.ok(loaded);
	const hud = renderHud(loaded, 30)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story5-resume-hud.txt"), "utf8")
		.trimEnd();
	assert.equal(hud, expected);
});

test("e2e: paused cue renders centered", () => {
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
		config: { dt: 1, gravity: 0, viewportWidth: 8 },
	});
	setPaused(state, true);
	const frame = renderViewport(state, 8, 4);
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story6-paused.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: paused cue renders in frame", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0 },
	});
	setPaused(state, true);
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story32-paused-frame.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: power up cue overlays", () => {
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
		config: { dt: 1, gravity: 0, viewportWidth: 8 },
	});
	state.cue = { text: "POWER UP", ttl: 1, persist: false };
	const frame = renderViewport(state, 8, 4);
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story7-powerup.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: particle renders", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 3,
		startY: 2,
		config: { dt: 1, gravity: 0 },
	});
	state.particles.push({ x: 1, y: 1, vx: 0, vy: 0, life: 1 });
	const frame = renderFrame(state)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story6-particles.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});

test("e2e: particle renders in viewport", () => {
	const level = makeLevel([
		"    ",
		"    ",
		"    ",
		"####",
	]);
	const state = createGame({
		level,
		startX: 10,
		startY: 10,
		config: { dt: 1, gravity: 0, viewportWidth: 4 },
	});
	state.cameraX = 1;
	state.particles.push({ x: 2, y: 1, vx: 0, vy: 0, life: 1 });
	const frame = renderViewport(state, 4, 4)
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n");
	const expected = fs
		.readFileSync(path.join(__dirname, "fixtures", "story31-particles-viewport.txt"), "utf8")
		.trimEnd();
	assert.equal(frame, expected);
});
