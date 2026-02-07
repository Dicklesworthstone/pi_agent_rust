/**
 * Ping game extension - play with /ping
 */

import type { ExtensionAPI, ExtensionCommandContext } from "@mariozechner/pi-coding-agent";
import { matchesKey, truncateToWidth, visibleWidth } from "@mariozechner/pi-tui";

const GAME_WIDTH = 24;
const GAME_HEIGHT = 14;
const PLAYER_X = 0;
const AI_X = GAME_WIDTH - 1;
const PADDLE_HEIGHT = 4;
const TICK_MS = 90;
const WIN_SCORE = 5;

const BALL_DELAY_FAST = 1;
const BALL_DELAY_SLOW = 1;
const VIM_BOOST_TICKS = 12;
const POINT_PAUSE_TICKS = 10;

const CELL_WIDTH = 2;

const PLAYER_STEP = 2;
const AI_STEP = 1;
const AI_MOVE_RATE = 2 / 3;

const PING_SAVE_TYPE = "ping-save";
const LEGACY_PING_SAVE_TYPE = "paddle-ball-save";
const PING_SAVE_TYPES = new Set([PING_SAVE_TYPE, LEGACY_PING_SAVE_TYPE]);

type Direction = -1 | 1;

interface Ball {
	x: number;
	y: number;
	vx: Direction;
	vy: Direction;
}

interface GameState {
	ball: Ball;
	ballDelay: number;
	ballCounter: number;
	playerY: number;
	aiY: number;
	playerScore: number;
	aiScore: number;
	rally: number;
	highRally: number;
	pointPauseTicks: number;
	gameOver: boolean;
}

const randomDirection = (): Direction => (Math.random() < 0.5 ? -1 : 1);

const createBall = (vx: Direction): Ball => ({
	x: Math.floor(GAME_WIDTH / 2),
	y: Math.floor(GAME_HEIGHT / 2),
	vx,
	vy: randomDirection(),
});

const createInitialState = (highRally = 0): GameState => ({
	ball: createBall(randomDirection()),
	ballDelay: BALL_DELAY_SLOW,
	ballCounter: 0,
	playerY: Math.floor((GAME_HEIGHT - PADDLE_HEIGHT) / 2),
	aiY: Math.floor((GAME_HEIGHT - PADDLE_HEIGHT) / 2),
	playerScore: 0,
	aiScore: 0,
	rally: 0,
	highRally,
	pointPauseTicks: 0,
	gameOver: false,
});

const cloneState = (state: GameState): GameState => ({
	...state,
	ball: { ...state.ball },
});

const clamp = (value: number, min: number, max: number): number => Math.max(min, Math.min(max, value));

class PingComponent {
	private state: GameState;
	private interval: ReturnType<typeof setInterval> | null = null;
	private onClose: () => void;
	private onSave: (state: GameState | null) => void;
	private tui: { requestRender: () => void };
	private cachedLines: string[] = [];
	private cachedWidth = 0;
	private version = 0;
	private cachedVersion = -1;
	private paused: boolean;
	private autoPausedForWidth = false;
	private resumeAfterWidth = false;
	private vimBoostPending = false;
	private playerBoostTicks = 0;
	private aiMoveAccumulator = 0;

	constructor(
		tui: { requestRender: () => void },
		onClose: () => void,
		onSave: (state: GameState | null) => void,
		savedState?: GameState,
	) {
		this.tui = tui;
		this.onClose = onClose;
		this.onSave = onSave;

		if (savedState && !savedState.gameOver) {
			this.state = {
				...savedState,
				ballDelay: savedState.ballDelay ?? BALL_DELAY_SLOW,
				ballCounter: savedState.ballCounter ?? 0,
				pointPauseTicks: savedState.pointPauseTicks ?? 0,
			};
			this.paused = true;
		} else {
			const highRally = savedState?.highRally ?? 0;
			this.state = createInitialState(highRally);
			this.paused = false;
			this.startLoop();
		}
	}

	private startLoop(): void {
		if (this.interval) return;
		this.interval = setInterval(() => {
			if (this.paused || this.state.gameOver) return;
			this.tick();
			this.version++;
			this.tui.requestRender();
		}, TICK_MS);
	}

	private stopLoop(): void {
		if (!this.interval) return;
		clearInterval(this.interval);
		this.interval = null;
	}

	private pauseForWidth(): void {
		if (this.autoPausedForWidth) return;
		this.autoPausedForWidth = true;
		this.resumeAfterWidth = !this.paused && !this.state.gameOver;
		if (this.resumeAfterWidth) {
			this.paused = true;
			this.stopLoop();
		}
	}

	private resumeFromWidthPause(): void {
		if (!this.autoPausedForWidth) return;
		this.autoPausedForWidth = false;
		if (this.resumeAfterWidth && !this.state.gameOver) {
			this.paused = false;
			this.startLoop();
		}
		this.resumeAfterWidth = false;
	}

	private tick(): void {
		if (this.state.pointPauseTicks > 0) {
			this.state.pointPauseTicks -= 1;
			return;
		}
		this.updateAI();
		if (this.state.ballCounter < this.state.ballDelay - 1) {
			this.state.ballCounter += 1;
			return;
		}
		this.state.ballCounter = 0;
		this.moveBall();
		if (this.state.gameOver) {
			this.stopLoop();
			return;
		}
		if (this.playerBoostTicks > 0) {
			if (this.state.ball.vx > 0) {
				this.playerBoostTicks -= 1;
				this.moveBall();
				if (this.state.gameOver) {
					this.stopLoop();
					return;
				}
			} else {
				this.playerBoostTicks = 0;
			}
		}
	}

	private updateAI(): void {
		this.aiMoveAccumulator += AI_MOVE_RATE;
		if (this.aiMoveAccumulator < 1) {
			return;
		}
		this.aiMoveAccumulator -= 1;

		const center = Math.floor((GAME_HEIGHT - PADDLE_HEIGHT) / 2);
		const target = this.state.ball.vx > 0 ? this.state.ball.y : center;
		const desired = clamp(target - Math.floor(PADDLE_HEIGHT / 2), 0, GAME_HEIGHT - PADDLE_HEIGHT);

		if (this.state.aiY < desired) {
			this.state.aiY = Math.min(desired, this.state.aiY + AI_STEP);
		} else if (this.state.aiY > desired) {
			this.state.aiY = Math.max(desired, this.state.aiY - AI_STEP);
		}
	}

	private moveBall(): void {
		const ball = this.state.ball;
		let nextX = ball.x + ball.vx;
		let nextY = ball.y + ball.vy;

		if (nextY < 0 || nextY >= GAME_HEIGHT) {
			ball.vy = (ball.vy === 1 ? -1 : 1) as Direction;
			nextY = ball.y + ball.vy;
		}

		if (ball.vx < 0 && nextX <= PLAYER_X) {
			if (nextX === PLAYER_X && this.isPaddleHit(this.state.playerY, nextY)) {
				ball.vx = 1;
				ball.vy = this.deflect(ball.vy, this.state.playerY, nextY);
				this.applyPlayerHitSpeed(this.state.playerY, nextY);
				ball.x = PLAYER_X + 1;
				ball.y = nextY;
				this.bumpRally();
				return;
			}
			if (nextX < PLAYER_X) {
				this.scorePoint("ai");
				return;
			}
		}

		if (ball.vx > 0 && nextX >= AI_X) {
			if (nextX === AI_X && this.isPaddleHit(this.state.aiY, nextY)) {
				ball.vx = -1;
				ball.vy = this.deflect(ball.vy, this.state.aiY, nextY);
				this.applyHitSpeed(this.state.aiY, nextY);
				ball.x = AI_X - 1;
				ball.y = nextY;
				this.bumpRally();
				return;
			}
			if (nextX > AI_X) {
				this.scorePoint("player");
				return;
			}
		}

		ball.x = nextX;
		ball.y = nextY;
	}

	private isPaddleHit(paddleY: number, ballY: number): boolean {
		return ballY >= paddleY && ballY < paddleY + PADDLE_HEIGHT;
	}

	private deflect(currentVy: Direction, paddleY: number, ballY: number): Direction {
		const center = paddleY + (PADDLE_HEIGHT - 1) / 2;
		if (ballY < center) return -1;
		if (ballY > center) return 1;
		return currentVy;
	}

	private ballDelayForHit(paddleY: number, ballY: number): number {
		const offset = ballY - paddleY;
		const centerLow = Math.floor((PADDLE_HEIGHT - 1) / 2);
		const centerHigh = Math.ceil((PADDLE_HEIGHT - 1) / 2);
		if (offset >= centerLow && offset <= centerHigh) {
			return BALL_DELAY_SLOW;
		}
		return BALL_DELAY_FAST;
	}

	private applyPlayerHitSpeed(paddleY: number, ballY: number): void {
		if (this.vimBoostPending) {
			this.state.ballDelay = BALL_DELAY_FAST;
			this.state.ballCounter = 0;
			this.playerBoostTicks = VIM_BOOST_TICKS;
			this.vimBoostPending = false;
			return;
		}
		this.applyHitSpeed(paddleY, ballY);
	}

	private applyHitSpeed(paddleY: number, ballY: number): void {
		this.state.ballDelay = this.ballDelayForHit(paddleY, ballY);
		this.state.ballCounter = 0;
	}

	private bumpRally(): void {
		this.state.rally += 1;
		if (this.state.rally > this.state.highRally) {
			this.state.highRally = this.state.rally;
		}
	}

	private scorePoint(winner: "player" | "ai"): void {
		if (winner === "player") {
			this.state.playerScore += 1;
		} else {
			this.state.aiScore += 1;
		}

		this.state.rally = 0;

		const direction = winner === "player" ? 1 : -1;
		this.state.ball = createBall(direction);
		this.state.ballDelay = BALL_DELAY_SLOW;
		this.state.ballCounter = 0;
		this.state.pointPauseTicks = POINT_PAUSE_TICKS;
		this.playerBoostTicks = 0;
		this.vimBoostPending = false;
		this.aiMoveAccumulator = 0;

		if (this.state.playerScore >= WIN_SCORE || this.state.aiScore >= WIN_SCORE) {
			this.state.gameOver = true;
		}
	}

	private togglePause(): void {
		this.paused = !this.paused;
		if (this.paused) {
			this.stopLoop();
		} else {
			this.startLoop();
		}
		this.version++;
		this.tui.requestRender();
	}

	private restartGame(): void {
		const highRally = this.state.highRally;
		this.state = createInitialState(highRally);
		this.paused = false;
		this.vimBoostPending = false;
		this.playerBoostTicks = 0;
		this.aiMoveAccumulator = 0;
		this.stopLoop();
		this.startLoop();
		this.onSave(null);
		this.version++;
		this.tui.requestRender();
	}

	handleInput(data: string): void {
		if (this.paused) {
			if (this.autoPausedForWidth) {
				if (matchesKey(data, "escape")) {
					this.dispose();
					this.onSave(cloneState(this.state));
					this.onClose();
					return;
				}
				if (data === "q" || data === "Q") {
					this.dispose();
					this.onSave(null);
					this.onClose();
					return;
				}
				return;
			}
			if (matchesKey(data, "escape") || data === "q" || data === "Q") {
				this.dispose();
				this.onClose();
				return;
			}
			this.paused = false;
			this.startLoop();
			this.version++;
			this.tui.requestRender();
			return;
		}

		if (matchesKey(data, "escape")) {
			this.dispose();
			this.onSave(cloneState(this.state));
			this.onClose();
			return;
		}

		if (data === "q" || data === "Q") {
			this.dispose();
			this.onSave(null);
			this.onClose();
			return;
		}

		if (this.state.gameOver && (data === "r" || data === "R" || data === " ")) {
			this.restartGame();
			return;
		}

		if (data === "p" || data === "P") {
			this.togglePause();
			return;
		}

		if (
			matchesKey(data, "up") ||
			data === "w" ||
			data === "W" ||
			data === "k" ||
			data === "K"
		) {
			this.state.playerY = clamp(this.state.playerY - PLAYER_STEP, 0, GAME_HEIGHT - PADDLE_HEIGHT);
			this.vimBoostPending = data === "k" || data === "K";
			this.version++;
			this.tui.requestRender();
			return;
		}

		if (
			matchesKey(data, "down") ||
			data === "s" ||
			data === "S" ||
			data === "j" ||
			data === "J"
		) {
			this.state.playerY = clamp(this.state.playerY + PLAYER_STEP, 0, GAME_HEIGHT - PADDLE_HEIGHT);
			this.vimBoostPending = data === "j" || data === "J";
			this.version++;
			this.tui.requestRender();
			return;
		}
	}

	invalidate(): void {
		this.cachedWidth = 0;
	}

	render(width: number): string[] {
		if (width === this.cachedWidth && this.cachedVersion === this.version) {
			return this.cachedLines;
		}

		const maxCells = Math.floor((width - 2) / CELL_WIDTH);
		if (maxCells < GAME_WIDTH) {
			this.pauseForWidth();
			const message = "Ping needs a wider terminal. Resize to resume.";
			const line = truncateToWidth(message, width);
			this.cachedLines = [line];
			this.cachedWidth = width;
			this.cachedVersion = this.version;
			return this.cachedLines;
		}

		this.resumeFromWidthPause();

		const renderWidth = Math.min(GAME_WIDTH, maxCells);
		const boxWidth = renderWidth * CELL_WIDTH;

		const color = (code: string, text: string) => `\x1b[${code}m${text}\x1b[0m`;
		const dim = (text: string) => color("2", text);
		const accent = (text: string) => color("1;36", text);
		const playerColor = (text: string) => color("1;32", text);
		const aiColor = (text: string) => color("1;31", text);
		const ballColor = (text: string) => color("1;33", text);
		const ballGlyph = ballColor("ππ");

		const lines: string[] = [];
		const topBorder = dim(`+${"-".repeat(boxWidth)}+`);

		lines.push(this.padLine(topBorder, width));

		const titleLine = `${accent("PING")}  ${dim(`First to ${WIN_SCORE}`)}`;
		const scoreLine =
			`Player: ${playerColor(String(this.state.playerScore))}  ` +
			`Pi: ${aiColor(String(this.state.aiScore))}  ` +
			`Rally: ${ballColor(String(this.state.rally))}  ` +
			`Best: ${dim(String(this.state.highRally))}`;

		lines.push(this.padLine(this.boxLine(titleLine, boxWidth), width));
		lines.push(this.padLine(this.boxLine(scoreLine, boxWidth), width));
		lines.push(this.padLine(this.boxLine(dim("-".repeat(boxWidth)), boxWidth), width));

		const showPointScore = this.state.pointPauseTicks > 0 && this.state.ball.y > 0;
		const pointScoreCells = showPointScore
			? [
					playerColor(String(this.state.playerScore).padEnd(CELL_WIDTH)),
					dim("- "),
					aiColor(String(this.state.aiScore).padEnd(CELL_WIDTH)),
			  ]
			: [];

		for (let y = 0; y < GAME_HEIGHT; y++) {
			const rowCells: string[] = [];
			for (let x = 0; x < renderWidth; x++) {
				if (x === this.state.ball.x && y === this.state.ball.y) {
					rowCells.push(ballGlyph);
					continue;
				}
				if (x === PLAYER_X && y >= this.state.playerY && y < this.state.playerY + PADDLE_HEIGHT) {
					rowCells.push(playerColor("||"));
					continue;
				}
				if (x === AI_X && y >= this.state.aiY && y < this.state.aiY + PADDLE_HEIGHT) {
					rowCells.push(aiColor("||"));
					continue;
				}
				rowCells.push("  ");
			}
			if (showPointScore && y === this.state.ball.y - 1) {
				const start = clamp(
					this.state.ball.x - Math.floor(pointScoreCells.length / 2),
					0,
					renderWidth - pointScoreCells.length,
				);
				for (let i = 0; i < pointScoreCells.length; i++) {
					rowCells[start + i] = pointScoreCells[i];
				}
			}
			lines.push(this.padLine(`|${rowCells.join("")}|`, width));
		}

		lines.push(this.padLine(this.boxLine(dim("-".repeat(boxWidth)), boxWidth), width));

		let footer: string;
		if (this.paused) {
			footer = `${accent("PAUSED")} - Press any key to resume, ${accent("Q")} to quit`;
		} else if (this.state.gameOver) {
			if (this.state.playerScore >= WIN_SCORE) {
				footer = `${playerColor("YOU WIN!")} - Press ${accent("R")} to restart, ${accent("Q")} to quit`;
			} else {
				footer = `${color("31;1", "GAME OVER")} - Press ${accent("R")} to restart, ${accent("Q")} to quit`;
			}
		} else {
			footer = `Up/Down or W/S move, ${accent("P")} pause, ${accent("ESC")} save`;
		}
		lines.push(this.padLine(this.boxLine(footer, boxWidth), width));
		lines.push(this.padLine(topBorder, width));

		this.cachedLines = lines;
		this.cachedWidth = width;
		this.cachedVersion = this.version;

		return lines;
	}

	private boxLine(content: string, width: number): string {
		const truncated = truncateToWidth(content, width);
		const padding = Math.max(0, width - visibleWidth(truncated));
		return `|${truncated}${" ".repeat(padding)}|`;
	}

	private padLine(line: string, width: number): string {
		const truncated = truncateToWidth(line, width);
		const padding = Math.max(0, width - visibleWidth(truncated));
		return truncated + " ".repeat(padding);
	}

	dispose(): void {
		this.stopLoop();
	}
}

export default function (pi: ExtensionAPI) {
	const runGame = async (_args: string, ctx: ExtensionCommandContext) => {
		if (!ctx.hasUI) {
			ctx.ui.notify("Ping requires interactive mode", "error");
			return;
		}

		const entries = ctx.sessionManager.getEntries();
		let savedState: GameState | undefined;
		for (let i = entries.length - 1; i >= 0; i--) {
			const entry = entries[i];
			if (entry.type === "custom" && PING_SAVE_TYPES.has(entry.customType)) {
				savedState = entry.data as GameState;
				break;
			}
		}

		await ctx.ui.custom((tui, _theme, _kb, done) => {
			return new PingComponent(
				tui,
				() => done(undefined),
				(state) => {
					pi.appendEntry(PING_SAVE_TYPE, state);
				},
				savedState,
			);
		});
	};

	pi.registerCommand("ping", {
		description: "Play Ping (Pong-style)!",
		handler: runGame,
	});

}
