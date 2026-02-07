/**
 * Tetris game extension - play with /tetris
 */

import type { ExtensionAPI, ExtensionCommandContext } from "@mariozechner/pi-coding-agent";
import { matchesKey, truncateToWidth, visibleWidth } from "@mariozechner/pi-tui";

const BOARD_WIDTH = 10;
const BOARD_HEIGHT = 20;
const TICK_MS = 50;
const LOCK_DELAY_TICKS = 10; // 0.5s at 50ms per tick
const CELL_WIDTH = 2;
const PREVIEW_COUNT = 3;

const TETRIS_SAVE_TYPE = "tetris-save";

// Tetromino definitions: each piece has rotations as [row][col] offsets from pivot
type Piece = { shape: number[][][]; color: string };

const PIECES: Record<string, Piece> = {
	I: {
		shape: [
			[[0, -1], [0, 0], [0, 1], [0, 2]],
			[[-1, 0], [0, 0], [1, 0], [2, 0]],
			[[0, -1], [0, 0], [0, 1], [0, 2]],
			[[-1, 0], [0, 0], [1, 0], [2, 0]],
		],
		color: "36", // cyan
	},
	O: {
		shape: [
			[[0, 0], [0, 1], [1, 0], [1, 1]],
			[[0, 0], [0, 1], [1, 0], [1, 1]],
			[[0, 0], [0, 1], [1, 0], [1, 1]],
			[[0, 0], [0, 1], [1, 0], [1, 1]],
		],
		color: "33", // yellow
	},
	T: {
		shape: [
			[[0, -1], [0, 0], [0, 1], [-1, 0]],
			[[-1, 0], [0, 0], [1, 0], [0, 1]],
			[[0, -1], [0, 0], [0, 1], [1, 0]],
			[[-1, 0], [0, 0], [1, 0], [0, -1]],
		],
		color: "35", // magenta
	},
	S: {
		shape: [
			[[0, 0], [0, 1], [-1, 1], [-1, 2]],
			[[-1, 0], [0, 0], [0, 1], [1, 1]],
			[[0, 0], [0, 1], [-1, 1], [-1, 2]],
			[[-1, 0], [0, 0], [0, 1], [1, 1]],
		],
		color: "32", // green
	},
	Z: {
		shape: [
			[[-1, 0], [-1, 1], [0, 1], [0, 2]],
			[[0, 0], [-1, 0], [-1, 1], [-2, 1]],
			[[-1, 0], [-1, 1], [0, 1], [0, 2]],
			[[0, 0], [-1, 0], [-1, 1], [-2, 1]],
		],
		color: "31", // red
	},
	J: {
		shape: [
			[[-1, -1], [0, -1], [0, 0], [0, 1]],
			[[-1, 0], [-1, 1], [0, 0], [1, 0]],
			[[0, -1], [0, 0], [0, 1], [1, 1]],
			[[-1, 0], [0, 0], [1, 0], [1, -1]],
		],
		color: "34", // blue
	},
	L: {
		shape: [
			[[0, -1], [0, 0], [0, 1], [-1, 1]],
			[[-1, 0], [0, 0], [1, 0], [1, 1]],
			[[0, -1], [0, 0], [0, 1], [1, -1]],
			[[-1, -1], [-1, 0], [0, 0], [1, 0]],
		],
		color: "38;5;208", // orange
	},
};

const PIECE_NAMES = Object.keys(PIECES);

interface FallingPiece {
	type: string;
	rotation: number;
	row: number;
	col: number;
}

interface GameState {
	board: (string | null)[][]; // color code or null for empty
	current: FallingPiece;
	queue: string[];
	held: string | null;
	canHold: boolean;
	score: number;
	lines: number;
	level: number;
	highScore: number;
	gameOver: boolean;
	tickCounter: number;
	lockDelay: number;
	clearingRows: number[];
	clearAnimTicks: number;
}

const color = (code: string, text: string): string => `\x1b[${code}m${text}\x1b[0m`;
const dim = (text: string): string => color("2", text);
const accent = (text: string): string => color("33;1", text);
const bold = (text: string): string => color("1", text);

const randomPiece = (): string => PIECE_NAMES[Math.floor(Math.random() * PIECE_NAMES.length)];

const generateBag = (): string[] => {
	const bag = [...PIECE_NAMES];
	for (let i = bag.length - 1; i > 0; i--) {
		const j = Math.floor(Math.random() * (i + 1));
		[bag[i], bag[j]] = [bag[j], bag[i]];
	}
	return bag;
};

const createEmptyBoard = (): (string | null)[][] =>
	Array.from({ length: BOARD_HEIGHT }, () => Array(BOARD_WIDTH).fill(null));

const createInitialState = (highScore = 0): GameState => {
	const queue = [...generateBag(), ...generateBag()];
	const current: FallingPiece = {
		type: queue.shift()!,
		rotation: 0,
		row: 0,
		col: Math.floor(BOARD_WIDTH / 2),
	};
	return {
		board: createEmptyBoard(),
		current,
		queue,
		held: null,
		canHold: true,
		score: 0,
		lines: 0,
		level: 1,
		highScore,
		gameOver: false,
		tickCounter: 0,
		lockDelay: 0,
		clearingRows: [],
		clearAnimTicks: 0,
	};
};

const cloneState = (state: GameState): GameState => ({
	...state,
	board: state.board.map((row) => [...row]),
	current: { ...state.current },
	queue: [...state.queue],
	clearingRows: [...state.clearingRows],
});

const getPieceCells = (piece: FallingPiece): [number, number][] => {
	const shape = PIECES[piece.type].shape[piece.rotation];
	return shape.map(([dr, dc]) => [piece.row + dr, piece.col + dc]);
};

const isValidPosition = (board: (string | null)[][], piece: FallingPiece): boolean => {
	const cells = getPieceCells(piece);
	for (const [r, c] of cells) {
		// Allow cells above board (r < 0) - they're in the spawn zone
		if (r >= BOARD_HEIGHT || c < 0 || c >= BOARD_WIDTH) return false;
		// Only check board collision for cells that are on the board
		if (r >= 0 && board[r][c] !== null) return false;
	}
	return true;
};

const getDropSpeed = (level: number): number => {
	// Frames between drops, decreases with level
	return Math.max(2, 20 - (level - 1) * 2);
};

class TetrisComponent {
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

	constructor(
		tui: { requestRender: () => void },
		onClose: () => void,
		onSave: (state: GameState | null) => void,
		savedState?: GameState,
	) {
		this.tui = tui;
		this.onClose = onClose;
		this.onSave = onSave;
		this.state = savedState ? cloneState(savedState) : createInitialState();
		this.paused = false;
		this.startLoop();
	}

	private startLoop(): void {
		if (this.interval) return;
		this.interval = setInterval(() => this.tick(), TICK_MS);
	}

	private stopLoop(): void {
		if (this.interval) {
			clearInterval(this.interval);
			this.interval = null;
		}
	}

	private tick(): void {
		if (this.paused || this.state.gameOver) return;

		// Handle line clear animation
		if (this.state.clearingRows.length > 0) {
			this.state.clearAnimTicks++;
			if (this.state.clearAnimTicks >= 6) {
				this.finalizeClear();
			}
			this.version++;
			this.tui.requestRender();
			return;
		}

		this.state.tickCounter++;
		const dropSpeed = getDropSpeed(this.state.level);
		const canFall = isValidPosition(this.state.board, { ...this.state.current, row: this.state.current.row + 1 });

		if (!canFall) {
			this.state.lockDelay++;
			if (this.state.lockDelay >= LOCK_DELAY_TICKS) {
				this.lockPiece();
				this.version++;
				this.tui.requestRender();
				return;
			}
		} else {
			this.state.lockDelay = 0;
		}

		if (this.state.tickCounter >= dropSpeed) {
			this.state.tickCounter = 0;
			if (canFall) {
				this.tryMove(1, 0);
			}
		}

		this.version++;
		this.tui.requestRender();
	}

	private tryMove(dr: number, dc: number): boolean {
		const newPiece = { ...this.state.current, row: this.state.current.row + dr, col: this.state.current.col + dc };
		if (isValidPosition(this.state.board, newPiece)) {
			this.state.current = newPiece;
			this.state.lockDelay = 0;
			return true;
		}
		return false;
	}

	private tryRotate(dir: 1 | -1): boolean {
		const newRotation = (this.state.current.rotation + dir + 4) % 4;
		const newPiece = { ...this.state.current, rotation: newRotation };

		// Try basic rotation
		if (isValidPosition(this.state.board, newPiece)) {
			this.state.current = newPiece;
			this.state.lockDelay = 0;
			return true;
		}

		// Wall kicks
		const kicks = [[0, -1], [0, 1], [0, -2], [0, 2], [-1, 0], [1, 0]];
		for (const [dr, dc] of kicks) {
			const kickedPiece = { ...newPiece, row: newPiece.row + dr, col: newPiece.col + dc };
			if (isValidPosition(this.state.board, kickedPiece)) {
				this.state.current = kickedPiece;
				this.state.lockDelay = 0;
				return true;
			}
		}
		return false;
	}

	private hardDrop(): void {
		let dropDistance = 0;
		while (this.tryMove(1, 0)) {
			dropDistance++;
		}
		this.state.score += dropDistance * 2;
		this.lockPiece();
	}

	private lockPiece(): void {
		const cells = getPieceCells(this.state.current);
		const pieceColor = PIECES[this.state.current.type].color;

		for (const [r, c] of cells) {
			if (r >= 0 && r < BOARD_HEIGHT && c >= 0 && c < BOARD_WIDTH) {
				this.state.board[r][c] = pieceColor;
			}
		}

		// Check for line clears
		const fullRows: number[] = [];
		for (let r = 0; r < BOARD_HEIGHT; r++) {
			if (this.state.board[r].every((cell) => cell !== null)) {
				fullRows.push(r);
			}
		}

		if (fullRows.length > 0) {
			this.state.clearingRows = fullRows;
			this.state.clearAnimTicks = 0;
		} else {
			this.spawnNext();
		}
	}

	private finalizeClear(): void {
		const clearedCount = this.state.clearingRows.length;

		// Remove cleared rows
		this.state.board = this.state.board.filter((_, i) => !this.state.clearingRows.includes(i));

		// Add new empty rows at top
		for (let i = 0; i < clearedCount; i++) {
			this.state.board.unshift(Array(BOARD_WIDTH).fill(null));
		}

		// Scoring: 100, 300, 500, 800 for 1-4 lines
		const lineScores = [0, 100, 300, 500, 800];
		this.state.score += (lineScores[clearedCount] || 800) * this.state.level;
		this.state.lines += clearedCount;

		// Level up every 10 lines
		const newLevel = Math.floor(this.state.lines / 10) + 1;
		if (newLevel > this.state.level) {
			this.state.level = newLevel;
		}

		this.state.clearingRows = [];
		this.state.clearAnimTicks = 0;
		this.spawnNext();
	}

	private spawnNext(): void {
		// Ensure queue has enough pieces
		while (this.state.queue.length < PREVIEW_COUNT + 1) {
			this.state.queue.push(...generateBag());
		}

		this.state.current = {
			type: this.state.queue.shift()!,
			rotation: 0,
			row: 0,
			col: Math.floor(BOARD_WIDTH / 2),
		};
		this.state.lockDelay = 0;
		this.state.canHold = true;

		// Check game over
		if (!isValidPosition(this.state.board, this.state.current)) {
			this.state.gameOver = true;
			if (this.state.score > this.state.highScore) {
				this.state.highScore = this.state.score;
			}
		}
	}

	private holdPiece(): void {
		if (!this.state.canHold) return;

		const currentType = this.state.current.type;
		if (this.state.held === null) {
			this.state.held = currentType;
			this.spawnNext();
		} else {
			const heldType = this.state.held;
			this.state.held = currentType;
			this.state.current = {
				type: heldType,
				rotation: 0,
				row: 0,
				col: Math.floor(BOARD_WIDTH / 2),
			};
		}
		this.state.canHold = false;
	}

	handleInput(data: string): void {
		if (this.state.gameOver) {
			if (matchesKey(data, "r") || data === "r" || data === "R") {
				this.state = createInitialState(this.state.highScore);
				this.version++;
				this.tui.requestRender();
			} else if (matchesKey(data, "q") || data === "q" || data === "Q" || matchesKey(data, "escape")) {
				this.dispose();
				this.onSave(null);
				this.onClose();
			}
			return;
		}

		if (matchesKey(data, "p") || data === "p" || data === "P") {
			this.paused = !this.paused;
			this.version++;
			this.tui.requestRender();
			return;
		}

		if (matchesKey(data, "escape")) {
			this.dispose();
			this.onSave(this.state);
			this.onClose();
			return;
		}

		if (matchesKey(data, "q") || data === "q" || data === "Q") {
			this.dispose();
			this.onSave(null);
			this.onClose();
			return;
		}

		if (this.paused) {
			this.paused = false;
			this.version++;
			this.tui.requestRender();
			return;
		}

		// Movement - left
		if (matchesKey(data, "left") || data === "a" || data === "A" || data === "h" || data === "H") {
			this.tryMove(0, -1);
		}
		// Movement - right
		else if (matchesKey(data, "right") || data === "d" || data === "D" || data === "l" || data === "L") {
			this.tryMove(0, 1);
		}
		// Soft drop - single step per keypress
		else if (matchesKey(data, "down") || data === "s" || data === "S" || data === "j" || data === "J") {
			if (this.tryMove(1, 0)) {
				this.state.score += 1;
			}
		}
		// Rotate clockwise
		else if (matchesKey(data, "up") || data === "w" || data === "W" || data === "k" || data === "K") {
			this.tryRotate(1);
		}
		// Rotate counter-clockwise
		else if (data === "z" || data === "Z" || data === "x" || data === "X") {
			this.tryRotate(-1);
		}
		// Hard drop
		else if (data === " ") {
			this.hardDrop();
		}
		// Hold piece
		else if (data === "c" || data === "C") {
			this.holdPiece();
		}

		this.version++;
		this.tui.requestRender();
	}

	render(width: number, _height: number): string[] {
		if (this.cachedVersion === this.version && this.cachedWidth === width) {
			return this.cachedLines;
		}

		const lines: string[] = [];
		const boardWidth = BOARD_WIDTH * CELL_WIDTH;
		const sideWidth = 12;
		const totalWidth = boardWidth + 2 + sideWidth + 3;

		// Title
		lines.push(this.padLine(bold("╔═══ TETRIS ═══╗"), width));
		lines.push("");

		// Build board with current piece
		const displayBoard: (string | null)[][] = this.state.board.map((row) => [...row]);
		
		// Add ghost piece
		let ghostRow = this.state.current.row;
		const ghostPiece = { ...this.state.current };
		while (isValidPosition(this.state.board, { ...ghostPiece, row: ghostRow + 1 })) {
			ghostRow++;
		}
		ghostPiece.row = ghostRow;
		if (ghostRow !== this.state.current.row) {
			const ghostCells = getPieceCells(ghostPiece);
			for (const [r, c] of ghostCells) {
				if (r >= 0 && r < BOARD_HEIGHT && c >= 0 && c < BOARD_WIDTH && displayBoard[r][c] === null) {
					displayBoard[r][c] = "ghost";
				}
			}
		}

		// Add current piece
		const currentCells = getPieceCells(this.state.current);
		const currentColor = PIECES[this.state.current.type].color;
		for (const [r, c] of currentCells) {
			if (r >= 0 && r < BOARD_HEIGHT && c >= 0 && c < BOARD_WIDTH) {
				displayBoard[r][c] = currentColor;
			}
		}

		// Header
		const scoreStr = `Score: ${this.state.score.toString().padStart(6)}`;
		const levelStr = `Lv ${this.state.level}`;
		const linesStr = `Lines: ${this.state.lines}`;
		
		lines.push(this.padLine(`┌${"─".repeat(boardWidth)}┐  ${dim("HOLD")}`, width));

		// Render held piece
		const heldPreview = this.renderMiniPiece(this.state.held, !this.state.canHold);

		// Render preview pieces
		const previews = this.state.queue.slice(0, PREVIEW_COUNT).map((type) => this.renderMiniPiece(type, false));

		// Main board rows
		for (let r = 0; r < BOARD_HEIGHT; r++) {
			let rowStr = "│";
			for (let c = 0; c < BOARD_WIDTH; c++) {
				const cell = displayBoard[r][c];
				const clearing = this.state.clearingRows.includes(r);
				
				if (clearing) {
					// Flash animation
					const flash = this.state.clearAnimTicks % 2 === 0;
					rowStr += flash ? color("47", "  ") : "  ";
				} else if (cell === null) {
					rowStr += dim("· ");
				} else if (cell === "ghost") {
					rowStr += dim("░░");
				} else {
					rowStr += color(cell, "██");
				}
			}
			rowStr += "│";

			// Side panel
			let sideContent = "";
			if (r === 0) {
				sideContent = heldPreview[0] || "";
			} else if (r === 1) {
				sideContent = heldPreview[1] || "";
			} else if (r === 3) {
				sideContent = dim("NEXT");
			} else if (r >= 4 && r < 4 + PREVIEW_COUNT * 3) {
				const previewIdx = Math.floor((r - 4) / 3);
				const previewRow = (r - 4) % 3;
				if (previewIdx < previews.length && previewRow < 2) {
					sideContent = previews[previewIdx][previewRow] || "";
				}
			} else if (r === 14) {
				sideContent = scoreStr;
			} else if (r === 15) {
				sideContent = levelStr;
			} else if (r === 16) {
				sideContent = linesStr;
			} else if (r === 18) {
				sideContent = `Hi: ${this.state.highScore}`;
			}

			lines.push(this.padLine(`${rowStr}  ${sideContent}`, width));
		}

		lines.push(this.padLine(`└${"─".repeat(boardWidth)}┘`, width));

		// Controls
		lines.push("");
		if (this.paused) {
			lines.push(this.padLine(`${accent("PAUSED")} - Press any key to resume`, width));
		} else if (this.state.gameOver) {
			lines.push(this.padLine(color("31;1", "GAME OVER") + ` - ${accent("R")} restart, ${accent("Q")} quit`, width));
		} else {
			lines.push(this.padLine(`←→/AD move │ ↑/W rotate │ ↓/S soft drop │ ${accent("SPACE")} hard drop`, width));
			lines.push(this.padLine(`${accent("C")} hold │ ${accent("P")} pause │ ${accent("ESC")} save & quit`, width));
		}

		this.cachedLines = lines;
		this.cachedWidth = width;
		this.cachedVersion = this.version;

		return lines;
	}

	private renderMiniPiece(type: string | null, faded: boolean): string[] {
		if (type === null) return ["    ", "    "];
		
		const piece = PIECES[type];
		const shape = piece.shape[0];
		const cells = new Set(shape.map(([r, c]) => `${r},${c}`));
		
		const rows: string[] = [];
		for (let r = -1; r <= 1; r++) {
			let rowStr = "";
			for (let c = -1; c <= 2; c++) {
				if (cells.has(`${r},${c}`)) {
					rowStr += faded ? dim("▓▓") : color(piece.color, "██");
				} else {
					rowStr += "  ";
				}
			}
			rows.push(rowStr);
		}
		return rows.slice(0, 2);
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
			ctx.ui.notify("Tetris requires interactive mode", "error");
			return;
		}

		const entries = ctx.sessionManager.getEntries();
		let savedState: GameState | undefined;
		for (let i = entries.length - 1; i >= 0; i--) {
			const entry = entries[i];
			if (entry.type === "custom" && entry.customType === TETRIS_SAVE_TYPE) {
				savedState = entry.data as GameState;
				break;
			}
		}

		await ctx.ui.custom((tui, _theme, _kb, done) => {
			return new TetrisComponent(
				tui,
				() => done(undefined),
				(state) => {
					pi.appendEntry(TETRIS_SAVE_TYPE, state);
				},
				savedState,
			);
		});
	};

	pi.registerCommand("tetris", {
		description: "Play Tetris!",
		handler: runGame,
	});
}
