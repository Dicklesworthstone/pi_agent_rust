/**
 * Mario-Not - a Mario-style TUI platformer. Play with /mario-not
 */

import type { ExtensionAPI, ExtensionCommandContext } from "@mariozechner/pi-coding-agent";
import { matchesKey, truncateToWidth, visibleWidth } from "@mariozechner/pi-tui";

const {
	createGame,
	stepGame,
	renderViewport,
	renderHud,
	saveState,
	loadState,
	setPaused,
	makeLevel,
} = require("./engine.js") as typeof import("./engine.js");
const { ALL_LEVELS } = require("./levels.js") as typeof import("./levels.js");
const { COLORS } = require("./colors.js") as { COLORS: Record<string, string> };
const { DEFAULT_CONFIG, LEVEL_INTRO_TIME } = require("./constants.js") as typeof import("./constants.js");

const TICK_MS = 25;
const VIEWPORT_W = 40;
const VIEWPORT_H = 15;
const HUD_LINES = 2;
const SAVE_TYPE = "mario-not-save";
const FRAME_INNER_WIDTH = 36;
const START_X = 1;
const START_Y = 13;

// Level metadata: name and theme color
const LEVEL_META: { name: string; frameColor: string; textColor: string }[] = [
	{ name: "WORLD 1-1", frameColor: COLORS.yellow, textColor: COLORS.cyan },
	{ name: "WORLD 1-2", frameColor: COLORS.yellow, textColor: COLORS.cyan },
	{ name: "WORLD 1-3", frameColor: COLORS.yellow, textColor: COLORS.cyan },
	{ name: "BOWSER'S CASTLE", frameColor: COLORS.red, textColor: COLORS.red },
];

class MarioNotComponent {
	private readonly tui: any;
	private readonly onClose: () => void;
	private readonly onSave: (state: any) => void;
	private interval: NodeJS.Timeout | null = null;
	private state: any;
	private version = 0;
	private cache = { lines: [] as string[], width: 0, version: -1 };
	private moveDir = 0;
	private runHeld = true;
	private jumpQueued = false;
	private autosaveTimer = 0;
	private levelClearTimer = 0;
	private introTimer = 0;
	private shownHazardAdvice = false;
	private readonly config: any;

	constructor(tui: any, onClose: () => void, onSave: (state: any) => void, saved?: any) {
		this.tui = tui;
		this.onClose = onClose;
		this.onSave = onSave;

		// Override default config with game-specific values
		this.config = {
			...DEFAULT_CONFIG,
			dt: TICK_MS / 1000,
			viewportWidth: VIEWPORT_W,
			walkSpeed: 5.2,
			runSpeed: 9.0,
			groundAccel: 70,
			groundDecel: 56,
			airAccel: 48,
			gravity: 35,
			maxFall: 15,
			jumpVel: 15,
		};
		const restored = saved ? loadState(saved, { config: this.config }) : null;
		if (restored) {
			this.state = restored;
		} else {
			this.state = createGame({ level: makeLevel(ALL_LEVELS[0]), startX: START_X, startY: START_Y, config: this.config, levelIndex: 1 });
			this.state.mode = "level_intro";
			this.introTimer = 0;
		}

		this.interval = setInterval(() => this.tick(), TICK_MS);
	}

	private tick(): void {
		// Handle level intro
		if (this.state.mode === "level_intro") {
			this.introTimer += TICK_MS / 1000;
			if (this.introTimer >= LEVEL_INTRO_TIME) {
				this.state.mode = "playing";
				this.introTimer = 0;
			}
			this.version += 1;
			this.tui.requestRender();
			return;
		}

		const input: any = {};
		if (this.moveDir < 0) input.left = true;
		if (this.moveDir > 0) input.right = true;
		if (this.runHeld) input.run = true;
		if (this.jumpQueued) input.jump = true;
		this.jumpQueued = false;

		const wasPlaying = this.state.mode === "playing";
		const prevSize = this.state.player.size;
		stepGame(this.state, input);

		// Show hazard advice on first death/damage in level 4 (castle with lava/fireballs)
		if (!this.shownHazardAdvice && this.state.levelIndex === 4 && wasPlaying) {
			const tookDamage = this.state.mode === "dead" || (prevSize === "big" && this.state.player.size === "small");
			if (tookDamage) {
				this.shownHazardAdvice = true;
				this.state.cue = { text: "TIP: [X] Walk  [S/Down/J] Stop", ttl: 3.0, persist: false };
			}
		}

		if (this.state.mode === "playing") {
			this.autosaveTimer += TICK_MS / 1000;
			if (this.autosaveTimer >= 5) {
				this.onSave(saveState(this.state));
				this.autosaveTimer = 0;
			}
		}

		// Handle level transition
		if (this.state.mode === "level_clear") {
			this.levelClearTimer += TICK_MS / 1000;
			if (this.levelClearTimer >= 2) {
				const nextLevel = this.state.levelIndex + 1;
				if (nextLevel <= ALL_LEVELS.length) {
					this.goToLevel(nextLevel, true);
				} else {
					this.state.mode = "victory";
				}
			}
		}

		this.version += 1;
		this.tui.requestRender();
	}

	private goToLevel(levelNum: number, keepProgress = false): void {
		if (levelNum < 1 || levelNum > ALL_LEVELS.length) return;
		const prev = this.state;
		this.state = createGame({
			level: makeLevel(ALL_LEVELS[levelNum - 1]),
			startX: START_X,
			startY: START_Y,
			config: this.config,
			levelIndex: levelNum,
		});
		if (keepProgress) {
			this.state.score = prev.score;
			this.state.coins = prev.coins;
			this.state.lives = prev.lives;
			this.state.player.size = prev.player.size;
		}
		this.state.mode = "level_intro";
		this.introTimer = 0;
		this.levelClearTimer = 0;
		this.version += 1;
		this.tui.requestRender();
	}

	private restart(): void {
		this.goToLevel(this.state.levelIndex);
	}

	handleInput(key: string): boolean {
		if (matchesKey(key, "escape") || key === "q" || key === "Q") {
			this.onSave(saveState(this.state));
			this.onClose();
		} else if (key >= "1" && key <= "9") {
			this.goToLevel(parseInt(key, 10));
		} else if (key === "r" || key === "R") {
			this.restart();
		} else if (key === "p" || key === "P") {
			const paused = this.state.mode === "paused";
			setPaused(this.state, !paused);
			if (this.state.mode === "paused") this.onSave(saveState(this.state));
			this.version += 1;
			this.tui.requestRender();
		} else if (key === "x" || key === "X") {
			this.runHeld = !this.runHeld;
		} else if (matchesKey(key, "up") || key === " " || key === "z" || key === "Z" || key === "k" || key === "K") {
			this.jumpQueued = true;
		} else if (matchesKey(key, "left") || key === "a" || key === "A" || key === "h" || key === "H") {
			this.moveDir = -1;
		} else if (matchesKey(key, "right") || key === "d" || key === "l" || key === "D" || key === "L") {
			this.moveDir = 1;
		} else if (matchesKey(key, "down") || key === "s" || key === "S" || key === "j" || key === "J") {
			this.moveDir = 0;
		}
		return true;
	}

	render(width: number, height: number): string[] {
		const pad = (line: string) => {
			const truncated = truncateToWidth(line, width);
			const padding = Math.max(0, width - visibleWidth(truncated));
			return truncated + " ".repeat(padding);
		};

		const minWidth = VIEWPORT_W * 2;
		const minHeight = VIEWPORT_H + HUD_LINES + 2;
		if (width < minWidth || height < minHeight) {
			return [
				"",
				pad("MARIO-NOT"),
				"",
				pad("Terminal too small"),
				pad(`Need ${minWidth} cols, ${minHeight} rows`),
				"",
				pad("[Q] Quit"),
			];
		}

		if (this.cache.version === this.version && this.cache.width === width) return this.cache.lines;

		const lines: string[] = [];

		// Level intro screen with cool frame
		if (this.state.mode === "level_intro") {
			const levelNum = this.state.levelIndex;
			const levelId = `1-${levelNum}`;
			const meta = LEVEL_META[levelNum - 1] || LEVEL_META[0];
			const { name: levelName, frameColor, textColor } = meta;
			const emptyRow = " ".repeat(FRAME_INNER_WIDTH);

			const livesDisplay = `${textColor}<>  x ${this.state.lives}${COLORS.reset}`;
			const livesLen = 6 + this.state.lives.toString().length;

			// Center content in frame (accounting for color codes)
			const centerText = (text: string, visLen: number) => {
				const leftPad = Math.floor((FRAME_INNER_WIDTH - visLen) / 2);
				const rightPad = FRAME_INNER_WIDTH - visLen - leftPad;
				return " ".repeat(leftPad) + text + " ".repeat(rightPad);
			};

			// Build retro frame
			lines.push("");
			lines.push("");
			lines.push(pad(`${frameColor}╔${"═".repeat(FRAME_INNER_WIDTH)}╗${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${emptyRow}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${centerText(`${textColor}${levelName}${COLORS.reset}`, levelName.length)}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${emptyRow}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${centerText(`${textColor}${levelId}${COLORS.reset}`, 3)}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${emptyRow}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${centerText(livesDisplay, livesLen)}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}║${COLORS.reset}${emptyRow}${frameColor}║${COLORS.reset}`));
			lines.push(pad(`${frameColor}╚${"═".repeat(FRAME_INNER_WIDTH)}╝${COLORS.reset}`));
			lines.push("");
			lines.push(pad(`${COLORS.gray}[1-4] Select level${COLORS.reset}`));
			lines.push("");

			this.cache = { lines, width, version: this.version };
			return lines;
		}

		lines.push(...renderHud(this.state, minWidth).split("\n").map(pad));
		lines.push(...renderViewport(this.state, VIEWPORT_W, VIEWPORT_H).split("\n").map(pad));
		lines.push("");
		const toggleHint = this.runHeld ? "[X] Walk" : "[X] Run";
		lines.push(pad(`[Arrows/HJKL] Move  [Space/K] Jump  ${toggleHint}  [P] Pause  [R] Restart  [1-4] Level  [Q] Quit`));
		if (this.state.mode === "game_over") lines.push(pad("GAME OVER - [Q] Quit"));
		if (this.state.mode === "victory") lines.push(pad("YOU WIN! Final Score: " + this.state.score + " - [Q] Quit"));

		this.cache = { lines, width, version: this.version };
		return lines;
	}

	dispose(): void {
		if (this.interval) clearInterval(this.interval);
	}
}

export default function (api: ExtensionAPI) {
	api.registerCommand("mario-not", {
		description: "Play Mario-Not (Mario-style platformer)!",
		handler: async (_args: string, ctx: ExtensionCommandContext) => {
			if (!ctx.hasUI) {
				ctx.ui.notify("Mario-Not requires interactive mode", "error");
				return;
			}

			const entries = ctx.sessionManager.getEntries();
			const saved = entries.reverse().find((e) => e.type === "custom" && e.customType === SAVE_TYPE)?.data as any | undefined;

			await ctx.ui.custom((tui, _theme, _kb, done) =>
				new MarioNotComponent(tui, () => done(undefined), (state) => api.appendEntry(SAVE_TYPE, state), saved)
			);
		},
	});
}
