/**
 * Picman - Pi eats tokens, avoids bugs! Play with /picman
 */

import type { ExtensionAPI, ExtensionCommandContext } from "@mariozechner/pi-coding-agent";
import { matchesKey, truncateToWidth, visibleWidth } from "@mariozechner/pi-tui";

const TICK_MS = 100;
const POWER_DURATION = 50;
const BUG_RESPAWN_TICKS = 30;
const INITIAL_LIVES = 3;
const BUG_MOVE_INTERVAL = 2;
const SAVE_TYPE = "picman-save";

type Direction = "up" | "down" | "left" | "right";

// The codebase - navigate and collect tokens!
const MAZE_TEMPLATE = [
	"#####################",
	"#.........#.........#",
	"#O###.###.#.###.###O#",
	"#.###.###.#.###.###.#",
	"#...................#",
	"#.###.#.#####.#.###.#",
	"#.....#...#...#.....#",
	"#####.### # ###.#####",
	"    #.#  B B  #.#    ",
	"#####.# BBB B#.#####",
	"     .  BBB B .     ",
	"#####.# BBBBB #.#####",
	"    #.#       #.#    ",
	"#####.# ##### #.#####",
	"#.........#.........#",
	"#.###.###.#.###.###.#",
	"#O..#.....P.....#..O#",
	"###.#.#.#####.#.#.###",
	"#.....#...#...#.....#",
	"#.#######.#.#######.#",
	"#####################",
];

const DIRECTIONS: Record<Direction, { x: number; y: number }> = {
	up: { x: 0, y: -1 }, down: { x: 0, y: 1 }, left: { x: -1, y: 0 }, right: { x: 1, y: 0 },
};
const OPPOSITE: Record<Direction, Direction> = { up: "down", down: "up", left: "right", right: "left" };

// ANSI colors
const [RESET, CYAN, BLUE, WHITE, DIM, RED] = ["\x1b[0m", "\x1b[1;96m", "\x1b[1;34m", "\x1b[1;97m", "\x1b[2m", "\x1b[1;91m"];
const GREEN = "\x1b[1;32m";
const BUG_COLORS = ["\x1b[1;91m", "\x1b[1;95m", "\x1b[1;93m", "\x1b[1;31m"]; // Red, Magenta, Yellow, Dark Red
const BUG_NAMES = ["Bug", "Error", "Crash", "Glitch"];

interface Bug {
	x: number; y: number; homeX: number; homeY: number;
	direction: Direction; color: string; name: string;
	squashed: boolean; respawnTimer: number;
}

interface GameState {
	pi: { x: number; y: number };
	piDir: Direction;
	nextDir: Direction | null;
	bugs: Bug[];
	maze: string[][];
	tokens: number; highScore: number; lives: number; level: number;
	tokensRemaining: number; caffeine: number; tickCount: number;
	gameOver: boolean; paused: boolean; mouthOpen: boolean;
	deathAnimation: number; levelCompleteTimer: number;
}

const findInMaze = (char: string): { x: number; y: number }[] => {
	const results: { x: number; y: number }[] = [];
	MAZE_TEMPLATE.forEach((row, y) => {
		for (let x = 0; x < row.length; x++) if (row[x] === char) results.push({ x, y });
	});
	return results;
};

const createMaze = (): string[][] => {
	const maze = MAZE_TEMPLATE.map(row => row.split(""));
	for (let y = 0; y < maze.length; y++)
		for (let x = 0; x < maze[y].length; x++)
			if (maze[y][x] === "P" || maze[y][x] === "B") maze[y][x] = " ";
	return maze;
};

const countTokens = (maze: string[][]): number =>
	maze.flat().filter(c => c === "." || c === "O").length;

const createBugs = (): Bug[] =>
	findInMaze("B").slice(0, 4).map((pos, i) => ({
		x: pos.x, y: pos.y, homeX: pos.x, homeY: pos.y,
		direction: "up" as Direction, color: BUG_COLORS[i], name: BUG_NAMES[i],
		squashed: false, respawnTimer: 0,
	}));

const createState = (highScore = 0): GameState => {
	const maze = createMaze();
	const piStart = findInMaze("P")[0] || { x: 10, y: 16 };
	return {
		pi: piStart, piDir: "right", nextDir: null,
		bugs: createBugs(), maze,
		tokens: 0, highScore, lives: INITIAL_LIVES, level: 1,
		tokensRemaining: countTokens(maze), caffeine: 0, tickCount: 0,
		gameOver: false, paused: false, mouthOpen: true,
		deathAnimation: 0, levelCompleteTimer: 0,
	};
};

const resetLevel = (s: GameState): void => {
	s.maze = createMaze();
	s.pi = findInMaze("P")[0] || { x: 10, y: 16 };
	s.piDir = "right"; s.nextDir = null;
	s.bugs = createBugs();
	s.tokensRemaining = countTokens(s.maze);
	s.caffeine = s.deathAnimation = s.levelCompleteTimer = 0;
};

const resetPositions = (s: GameState): void => {
	s.pi = findInMaze("P")[0] || { x: 10, y: 16 };
	s.piDir = "right"; s.nextDir = null;
	s.caffeine = s.deathAnimation = 0;
	s.bugs.forEach(b => { b.x = b.homeX; b.y = b.homeY; b.squashed = false; b.respawnTimer = 0; });
};

const isWalkable = (maze: string[][], x: number, y: number): boolean =>
	y >= 0 && y < maze.length && (x < 0 || x >= maze[0].length || maze[y][x] !== "#");

const wrap = (x: number, w: number): number => x < 0 ? w - 1 : x >= w ? 0 : x;

const dist = (a: { x: number; y: number }, b: { x: number; y: number }): number =>
	Math.abs(a.x - b.x) + Math.abs(a.y - b.y);

const getValidDirs = (maze: string[][], x: number, y: number, exclude?: Direction): Direction[] =>
	(Object.keys(DIRECTIONS) as Direction[]).filter(dir => {
		if (dir === exclude) return false;
		const d = DIRECTIONS[dir];
		return isWalkable(maze, x + d.x, y + d.y);
	});

const moveBug = (b: Bug, s: GameState): void => {
	if (b.squashed) {
		if (--b.respawnTimer <= 0) { b.squashed = false; b.x = b.homeX; b.y = b.homeY; }
		return;
	}

	const { maze, pi, caffeine } = s;
	const validDirs = getValidDirs(maze, b.x, b.y, OPPOSITE[b.direction]);

	if (validDirs.length === 0) {
		const rev = OPPOSITE[b.direction], d = DIRECTIONS[rev];
		if (isWalkable(maze, b.x + d.x, b.y + d.y)) {
			b.direction = rev; b.x = wrap(b.x + d.x, maze[0].length); b.y += d.y;
		}
		return;
	}

	// Target: flee if Pi is caffeinated, else chase with personality
	let target = pi;
	if (caffeine <= 0) {
		if (b.name === "Error") {
			const d = DIRECTIONS[s.piDir];
			target = { x: pi.x + d.x * 4, y: pi.y + d.y * 4 };
		} else if (b.name === "Glitch" && dist(b, pi) < 8) {
			target = { x: 0, y: maze.length - 1 };
		}
	}

	// Pick best direction
	const bestDir = validDirs.reduce((best, dir) => {
		const d = DIRECTIONS[dir];
		const nx = wrap(b.x + d.x, maze[0].length), ny = b.y + d.y;
		const score = dist({ x: nx, y: ny }, target);
		const bestScore = dist({ x: wrap(b.x + DIRECTIONS[best].x, maze[0].length), y: b.y + DIRECTIONS[best].y }, target);
		return caffeine > 0 ? (score > bestScore ? dir : best) : (score < bestScore ? dir : best);
	}, validDirs[0]);

	b.direction = bestDir;
	const d = DIRECTIONS[bestDir];
	b.x = wrap(b.x + d.x, maze[0].length); b.y += d.y;
};

const movePi = (s: GameState): void => {
	const { maze, pi } = s;

	// Try queued direction
	if (s.nextDir) {
		const d = DIRECTIONS[s.nextDir];
		if (isWalkable(maze, pi.x + d.x, pi.y + d.y)) { s.piDir = s.nextDir; s.nextDir = null; }
	}

	const d = DIRECTIONS[s.piDir];
	if (isWalkable(maze, pi.x + d.x, pi.y + d.y)) {
		pi.x = wrap(pi.x + d.x, maze[0].length); pi.y += d.y;
		const cell = maze[pi.y][pi.x];
		if (cell === ".") { maze[pi.y][pi.x] = " "; s.tokens += 10; s.tokensRemaining--; }
		else if (cell === "O") { maze[pi.y][pi.x] = " "; s.tokens += 50; s.tokensRemaining--; s.caffeine = POWER_DURATION; }
	}

	if (s.tickCount % 3 === 0) s.mouthOpen = !s.mouthOpen;
};

const checkCollisions = (s: GameState): void => {
	for (const b of s.bugs) {
		if (b.squashed || b.x !== s.pi.x || b.y !== s.pi.y) continue;
		if (s.caffeine > 0) { b.squashed = true; b.respawnTimer = BUG_RESPAWN_TICKS; s.tokens += 200; }
		else if (--s.lives <= 0) s.gameOver = true;
		else s.deathAnimation = 10;
	}
};

class PicmanComponent {
	private state: GameState;
	private interval: ReturnType<typeof setInterval> | null = null;
	private onClose: () => void;
	private onSave: (state: GameState) => void;
	private tui: { requestRender: () => void };
	private cache = { lines: [] as string[], width: 0, version: -1 };
	private version = 0;

	constructor(tui: { requestRender: () => void }, onClose: () => void, onSave: (s: GameState) => void, saved?: GameState) {
		this.tui = tui; this.onClose = onClose; this.onSave = onSave;
		this.state = saved ? { ...saved, paused: true } : createState();
		this.interval = setInterval(() => this.tick(), TICK_MS);
	}

	private tick(): void {
		const s = this.state;
		if (s.paused || s.gameOver) return;
		s.tickCount++;

		if (s.deathAnimation > 0) { if (--s.deathAnimation === 0) resetPositions(s); }
		else if (s.levelCompleteTimer > 0) { if (--s.levelCompleteTimer === 0) { s.level++; resetLevel(s); } }
		else if (s.tokensRemaining === 0) s.levelCompleteTimer = 20;
		else {
			if (s.caffeine > 0) s.caffeine--;
			movePi(s);
			const bugInterval = Math.max(1, BUG_MOVE_INTERVAL - Math.floor(s.level / 3));
			if (s.tickCount % bugInterval === 0) s.bugs.forEach(b => moveBug(b, s));
			checkCollisions(s);
			if (s.tokens > s.highScore) s.highScore = s.tokens;
		}

		this.version++;
		this.tui.requestRender();
	}

	handleInput(key: string): boolean {
		const s = this.state;
		if (matchesKey(key, "escape") || key === "q" || key === "Q") { this.onSave(s); this.onClose(); return true; }
		if (key === "n" || key === "N") { Object.assign(this.state, createState(Math.max(s.highScore, s.tokens))); this.version++; this.tui.requestRender(); return true; }
		if (key === " " || key === "p" || key === "P") { s.paused = !s.paused; this.version++; this.tui.requestRender(); return true; }
		if (s.gameOver || s.paused) return true;

		const dirMap: Record<string, Direction> = { k: "up", w: "up", j: "down", s: "down", h: "left", a: "left", l: "right", d: "right" };
		if (matchesKey(key, "up")) s.nextDir = "up";
		else if (matchesKey(key, "down")) s.nextDir = "down";
		else if (matchesKey(key, "left")) s.nextDir = "left";
		else if (matchesKey(key, "right")) s.nextDir = "right";
		else if (dirMap[key]) s.nextDir = dirMap[key];
		return true;
	}

	render(width: number, _height: number): string[] {
		const pad = (line: string) => truncateToWidth(line, width) + " ".repeat(Math.max(0, width - visibleWidth(truncateToWidth(line, width))));
		const minWidth = MAZE_TEMPLATE[0].length * 2 + 4;

		if (width < minWidth) {
			return ["", pad(`${CYAN}PICMAN${RESET}`), "", pad("Terminal too narrow"), pad(`Need ${minWidth} cols`), "", pad("[Q] Quit")];
		}

		if (this.cache.version === this.version && this.cache.width === width) return this.cache.lines;

		const s = this.state, lines: string[] = [""];
		const lives = Array(Math.max(0, s.lives)).fill(`${CYAN}Pi${RESET}`).join(" ");
		lines.push(pad(`${CYAN}PICMAN${RESET}  Tokens: ${WHITE}${s.tokens}${RESET}  Hi: ${s.highScore}  Lv: ${s.level}  ${lives}`));
		lines.push("");

		if (s.gameOver) {
			lines.push(pad(`${RED}SEGFAULT${RESET}`), pad(`Final Tokens: ${s.tokens}`), "", pad("[N] New Game  [Q] Quit"), "");
		} else {
			if (s.paused) lines.push(pad(`${DIM}PAUSED${RESET}`), pad("[SPACE] Resume  [N] New  [Q] Quit"), "");
			if (s.levelCompleteTimer > 0) lines.push(pad(`${GREEN}LEVEL ${s.level} SHIPPED!${RESET}`), "");
			if (s.deathAnimation > 0) lines.push(pad(`${RED}BUG FOUND!${RESET}`), "");

			for (let y = 0; y < s.maze.length; y++) {
				let row = "";
				for (let x = 0; x < s.maze[y].length; x++) {
					const cell = s.maze[y][x];
					if (s.pi.x === x && s.pi.y === y && s.deathAnimation === 0) {
						const glyph = s.mouthOpen ? ({ right: "Pi", left: "iP", up: "Pi", down: "Pi" })[s.piDir] : "PI";
						row += `${CYAN}${glyph}${RESET}`;
					} else {
						const bug = s.bugs.find(b => b.x === x && b.y === y && !b.squashed);
						if (bug) {
							const scared = s.caffeine > 0, blink = scared && s.caffeine < 15 && s.tickCount % 4 < 2;
							row += `${blink ? WHITE : scared ? GREEN : bug.color}${scared ? "><" : "<>"}${RESET}`;
						} else {
							row += cell === "#" ? `${BLUE}██${RESET}` : cell === "." ? ` ${DIM};${RESET}` : cell === "O" ? `${GREEN}()${RESET}` : "  ";
						}
					}
				}
				lines.push(pad(row));
			}

			lines.push("", pad(s.caffeine > 0 ? `${GREEN}CAFFEINATED!${RESET} [Arrows/HJKL] Move  [SPACE] Pause  [Q] Quit` : "[Arrows/HJKL] Move  [SPACE] Pause  [N] New  [Q] Quit"), "");
		}

		this.cache = { lines, width, version: this.version };
		return lines;
	}

	dispose(): void { if (this.interval) clearInterval(this.interval); }
}

export default function (api: ExtensionAPI) {
	api.registerCommand("picman", {
		description: "Play Picman! Collect tokens, squash bugs, drink coffee!",
		handler: async (_args, ctx: ExtensionCommandContext) => {
			if (!ctx.hasUI) { ctx.ui.notify("Picman requires interactive mode", "error"); return; }

			const entries = ctx.sessionManager.getEntries();
			const saved = entries.reverse().find(e => e.type === "custom" && e.customType === SAVE_TYPE)?.data as GameState | undefined;

			await ctx.ui.custom((tui, _theme, _kb, done) => new PicmanComponent(tui, () => done(undefined), s => api.appendEntry(SAVE_TYPE, s), saved));
		},
	});
}
