/**
 * sPIce Invaders game extension - play with /spice-invaders
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { matchesKey, truncateToWidth, visibleWidth } from "@mariozechner/pi-tui";

const GAME_WIDTH = 24;
const GAME_HEIGHT = 16;
const PLAYER_Y = GAME_HEIGHT - 1;
const TICK_MS = 100;

const INVADER_ROWS = 3;
const INVADER_COLS = 6;
const INVADER_START_X = 1;
const INVADER_START_Y = 1;
const INVADER_SPACING_X = 2;
const INVADER_SPACING_Y = 1;
const INITIAL_INVADER_COUNT = INVADER_ROWS * INVADER_COLS;

const INITIAL_LIVES = 3;
const BASE_INVADER_DELAY = 8;
const PLAYER_SHOT_DELAY = 1;
const MAX_PLAYER_BULLETS = 3;
const PLAYER_MOVE_STEP = 1;
const PLAYER_MOVE_HOLD_TICKS = 3;
const INVADER_FIRE_DELAY = 6;
const INVADER_BULLET_STEP_TICKS = 2;
const MAX_INVADER_BULLETS = 2;
const INVADER_SCORE = 10;
const INVADER_ROW_SCORES = [30, 20, 10];
const UFO_SCORE = 50;
const UFO_BASE_COOLDOWN = 70;
const READY_TICKS = 20;

const BOSS_WIDTH = 6;
const BOSS_HEIGHT = 2;
const BOSS_HP = 20;
const BOSS_MOVE_DELAY = 6;
const BOSS_BULLET_STEP_TICKS = 4;
const BOSS_MAX_BULLETS = 4;
const BOSS_FIRE_CHANCE_BONUS = 0.2;
const BOSS_SCORE = 200;
const BOSS_Y = 1;
const CHEAT_CODE = "clawd";
const CHEAT_BUFFER_TICKS = 12;
const BOSS_ENRAGE_RATIO = 0.5;
const BOSS_ENRAGE_TICKS = 16;

const CELL_WIDTH = 2;
const MIN_RENDER_CELLS = 10;

const SPACE_INVADERS_SAVE_TYPE = "spice-invaders-save";

const UFO_Y = 0;

const BOSS_FRAMES = [
	[
		["<\\", "()", "==", "==", "()", "/>"],
		["()", "\\/", "/\\", "/\\", "\\/", "()"],
	],
	[
		["<\\", "()", "~~", "~~", "()", "/>"],
		["()", "\\/", "/\\", "/\\", "\\/", "()"],
	],
] as const;

type Direction = -1 | 1;
type MoveDir = Direction | 0;
type Point = { x: number; y: number };

type BulletSource = "player" | "invader";

interface Bullet {
	x: number;
	y: number;
	from: BulletSource;
	unblockable?: boolean;
}

interface UfoState {
	x: number;
	dir: Direction;
	active: boolean;
}

interface BossState {
	active: boolean;
	x: number;
	y: number;
	dir: Direction;
	hp: number;
	maxHp: number;
	frame: 0 | 1;
	moveCounter: number;
}

interface ScatterInvader {
	x: number;
	y: number;
	vx: Direction;
	vy: Direction;
}

type BossIntroPhase = "scatter" | "descend" | null;

interface GameState {
	invaders: Point[];
	invaderDir: Direction;
	invaderFrame: 0 | 1;
	invaderMoveDelay: number;
	invaderMoveCounter: number;
	invaderOffsetY: number;
	playerX: number;
	playerBullets: Bullet[];
	invaderBullets: Bullet[];
	playerCooldown: number;
	invaderCooldown: number;
	playerMoveDir: MoveDir;
	playerMoveHold: number;
	bulletTick: number;
	wavePauseTicks: number;
	pendingWave: boolean;
	score: number;
	highScore: number;
	lives: number;
	level: number;
	gameOver: boolean;
	ufo: UfoState;
	ufoCooldown: number;
	boss: BossState;
	bossEnrageTicks: number;
	bossEnrageBlink: boolean;
	bossIntroPhase: BossIntroPhase;
	scatterInvaders: ScatterInvader[];
}

const createInvaders = (): Point[] => {
	const invaders: Point[] = [];
	for (let row = 0; row < INVADER_ROWS; row++) {
		for (let col = 0; col < INVADER_COLS; col++) {
			const x = INVADER_START_X + col * (1 + INVADER_SPACING_X);
			const y = INVADER_START_Y + row * (1 + INVADER_SPACING_Y);
			invaders.push({ x, y });
		}
	}
	return invaders;
};

const createBossState = (level = 1): BossState => {
	const maxHp = BOSS_HP * Math.max(1, level);
	return {
		active: false,
		x: 0,
		y: BOSS_Y,
		dir: 1,
		hp: maxHp,
		maxHp,
		frame: 0,
		moveCounter: 0,
	};
};

const bossEnrageHp = (boss: BossState): number => Math.max(1, Math.ceil(boss.maxHp * BOSS_ENRAGE_RATIO));

const bossIsEnraged = (boss: BossState): boolean => boss.active && boss.hp <= bossEnrageHp(boss);

const bossMoveDelayFor = (boss: BossState, level: number): number => {
	const levelBoost = Math.max(1, level);
	const base = Math.max(1, Math.floor(BOSS_MOVE_DELAY / levelBoost));
	return bossIsEnraged(boss) ? Math.max(1, Math.floor(base / 1.5)) : base;
};

const invaderDelayFor = (level: number, remaining: number): number => {
	const cleared = Math.max(0, INITIAL_INVADER_COUNT - remaining);
	const speedUp = Math.floor(cleared / 4);
	const levelBoost = Math.floor((level - 1) / 2);
	return Math.max(2, BASE_INVADER_DELAY - speedUp - levelBoost);
};

const invaderFireDelayFor = (level: number, remaining: number): number => {
	const cleared = Math.max(0, INITIAL_INVADER_COUNT - remaining);
	const speedUp = Math.floor(cleared / 6);
	const levelBoost = Math.floor((level - 1) / 3);
	return Math.max(2, INVADER_FIRE_DELAY - speedUp - levelBoost);
};

const invaderFireChanceFor = (remaining: number): number => {
	const ratio = 1 - remaining / INITIAL_INVADER_COUNT;
	return Math.min(0.75, 0.35 + ratio * 0.35);
};

const bossFireDelayFor = (level: number, boss: BossState): number => {
	const base = invaderFireDelayFor(level, 1);
	return bossIsEnraged(boss) ? Math.max(1, Math.floor(base / 1.5)) : base;
};

const bossFireChanceFor = (boss: BossState): number => {
	const base = Math.min(0.9, invaderFireChanceFor(1) + BOSS_FIRE_CHANCE_BONUS);
	return bossIsEnraged(boss) ? Math.min(0.95, base * 1.5) : base;
};

const bossBulletStepFor = (level: number, boss: BossState): number => {
	const levelBoost = Math.max(1, level);
	const base = Math.max(1, Math.floor(BOSS_BULLET_STEP_TICKS / levelBoost));
	return bossIsEnraged(boss) ? Math.max(1, Math.floor(base / 1.5)) : base;
};

const invaderRowScoreFor = (invader: Point, offsetY: number): number => {
	const spacing = 1 + INVADER_SPACING_Y;
	const rawRow = Math.round((invader.y - offsetY - INVADER_START_Y) / spacing);
	const row = Math.max(0, Math.min(INVADER_ROW_SCORES.length - 1, rawRow));
	return INVADER_ROW_SCORES[row] ?? INVADER_SCORE;
};

const createInitialState = (highScore = 0): GameState => {
	const invaders = createInvaders();
	return {
		invaders,
		invaderDir: 1,
		invaderFrame: 0,
		invaderMoveDelay: invaderDelayFor(1, invaders.length),
		invaderMoveCounter: 0,
		invaderOffsetY: 0,
		playerX: Math.floor(GAME_WIDTH / 2),
		playerBullets: [],
		invaderBullets: [],
		playerCooldown: 0,
		invaderCooldown: invaderFireDelayFor(1, invaders.length),
		playerMoveDir: 0,
		playerMoveHold: 0,
		bulletTick: 0,
		wavePauseTicks: 0,
		pendingWave: false,
		score: 0,
		highScore,
		lives: INITIAL_LIVES,
		level: 1,
		gameOver: false,
		ufo: { x: 0, dir: 1, active: false },
		ufoCooldown: UFO_BASE_COOLDOWN,
		boss: createBossState(1),
		bossEnrageTicks: 0,
		bossEnrageBlink: false,
		bossIntroPhase: null,
		scatterInvaders: [],
	};
};

const cloneState = (state: GameState): GameState => ({
	...state,
	invaders: state.invaders.map((invader) => ({ ...invader })),
	playerBullets: state.playerBullets.map((bullet) => ({ ...bullet })),
	invaderBullets: state.invaderBullets.map((bullet) => ({ ...bullet })),
	ufo: { ...state.ufo },
	boss: { ...state.boss },
	bossEnrageTicks: state.bossEnrageTicks,
	bossEnrageBlink: state.bossEnrageBlink,
	bossIntroPhase: state.bossIntroPhase,
	scatterInvaders: state.scatterInvaders.map((invader) => ({ ...invader })),
});

const normalizeState = (state: GameState): GameState => {
	const invaders = state.invaders ?? createInvaders();
	const level = state.level ?? 1;
	const boss = state.boss
		? {
				...state.boss,
				y: state.boss.y ?? BOSS_Y,
				maxHp: state.boss.maxHp ?? BOSS_HP * Math.max(1, level),
			}
		: createBossState(level);
	return {
		...state,
		invaders,
		ufo: state.ufo ?? { x: 0, dir: 1, active: false },
		ufoCooldown: state.ufoCooldown ?? UFO_BASE_COOLDOWN,
		invaderOffsetY: state.invaderOffsetY ?? 0,
		playerMoveDir: state.playerMoveDir ?? 0,
		playerMoveHold: state.playerMoveHold ?? 0,
		bulletTick: state.bulletTick ?? 0,
		wavePauseTicks: state.wavePauseTicks ?? 0,
		pendingWave: state.pendingWave ?? false,
		invaderCooldown: state.invaderCooldown ?? invaderFireDelayFor(level, invaders.length),
		boss,
		bossEnrageTicks: state.bossEnrageTicks ?? 0,
		bossEnrageBlink: state.bossEnrageBlink ?? false,
		bossIntroPhase: state.bossIntroPhase ?? null,
		scatterInvaders: state.scatterInvaders ?? [],
	};
};

class SpaceInvadersComponent {
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
	private cheatBuffer = "";
	private cheatBufferTicks = 0;

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
			this.state = normalizeState(savedState);
			this.paused = true;
		} else {
			const highScore = savedState?.highScore ?? 0;
			this.state = createInitialState(highScore);
			this.paused = false;
			this.startLoop();
		}
	}

	private startLoop(): void {
		if (this.interval) return;
		this.interval = setInterval(() => {
			if (this.paused || this.state.gameOver) return;
			this.tick();
			this.markDirty();
		}, TICK_MS);
	}

	private stopLoop(): void {
		if (!this.interval) return;
		clearInterval(this.interval);
		this.interval = null;
	}

	private markDirty(): void {
		this.version++;
		this.tui.requestRender();
	}

	private tick(): void {
		this.decayCheatBuffer();
		if (this.state.bossIntroPhase) {
			this.updateBossIntro();
			return;
		}
		if (this.state.bossEnrageTicks > 0) {
			this.state.bossEnrageTicks -= 1;
			this.state.bossEnrageBlink = !this.state.bossEnrageBlink;
			if (this.state.bossEnrageTicks === 0) {
				this.state.bossEnrageBlink = false;
			}
			return;
		}
		if (this.state.wavePauseTicks > 0) {
			this.state.wavePauseTicks -= 1;
			if (this.state.wavePauseTicks === 0 && this.state.pendingWave) {
				this.startNextWave();
			}
			return;
		}

		this.state.bulletTick += 1;
		if (this.state.playerCooldown > 0) this.state.playerCooldown--;
		if (this.state.invaderCooldown > 0) this.state.invaderCooldown--;

		this.applyHeldMovement();
		if (this.state.boss.active) {
			this.updateBoss();
		} else {
			this.updateUfo();
		}
		this.moveBullets();
		this.resolveBulletCollisions();
		if (this.state.gameOver) {
			this.stopLoop();
			return;
		}
		if (this.state.boss.active) {
			this.maybeFireInvaderBullet();
			return;
		}
		this.moveInvaders();
		if (this.state.gameOver) {
			this.stopLoop();
			return;
		}
		this.maybeFireInvaderBullet();
		this.maybeAdvanceWave();
	}

	private decayCheatBuffer(): void {
		if (this.cheatBufferTicks <= 0) return;
		this.cheatBufferTicks -= 1;
		if (this.cheatBufferTicks <= 0) {
			this.cheatBuffer = "";
		}
	}

	private registerCheatInput(data: string): boolean {
		if (data.length !== 1) return false;
		const lower = data.toLowerCase();
		if (lower < "a" || lower > "z") return false;
		this.cheatBuffer = (this.cheatBuffer + lower).slice(-CHEAT_CODE.length);
		this.cheatBufferTicks = CHEAT_BUFFER_TICKS;
		if (!this.cheatBuffer.endsWith(CHEAT_CODE)) return false;
		this.cheatBuffer = "";
		this.activateBoss();
		return true;
	}

	private activateBoss(): void {
		if (this.state.boss.active || this.state.bossIntroPhase) return;
		const centered = Math.floor((GAME_WIDTH - BOSS_WIDTH) / 2);
		const maxHp = BOSS_HP * Math.max(1, this.state.level);
		const scatterInvaders = this.state.invaders.map((invader) => ({
			x: invader.x,
			y: invader.y,
			vx: Math.random() < 0.5 ? -1 : 1,
			vy: Math.random() < 0.5 ? -1 : 1,
		}));

		this.state.boss = {
			active: false,
			x: Math.max(0, Math.min(GAME_WIDTH - BOSS_WIDTH, centered)),
			y: -BOSS_HEIGHT,
			dir: 1,
			hp: maxHp,
			maxHp,
			frame: 0,
			moveCounter: 0,
		};
		this.state.invaders = [];
		this.state.scatterInvaders = scatterInvaders;
		this.state.invaderBullets = [];
		this.state.playerBullets = [];
		this.state.invaderOffsetY = 0;
		this.state.invaderMoveCounter = 0;
		this.state.pendingWave = false;
		this.state.wavePauseTicks = 0;
		this.state.bossEnrageTicks = 0;
		this.state.bossEnrageBlink = false;
		this.state.bossIntroPhase = scatterInvaders.length ? "scatter" : "descend";
		this.state.playerMoveDir = 0;
		this.state.playerMoveHold = 0;
		this.state.ufo.active = false;
		this.state.ufoCooldown = UFO_BASE_COOLDOWN;
		this.state.invaderCooldown = bossFireDelayFor(this.state.level, this.state.boss);
		this.markDirty();
	}

	private updateBossIntro(): void {
		if (this.state.bossIntroPhase === "scatter") {
			this.updateScatterInvaders();
			if (this.state.scatterInvaders.length === 0) {
				this.state.bossIntroPhase = "descend";
			}
			return;
		}
		if (this.state.bossIntroPhase === "descend") {
			this.state.boss.y += 1;
			if (this.state.boss.y >= BOSS_Y) {
				this.state.boss.y = BOSS_Y;
				this.state.boss.active = true;
				this.state.bossIntroPhase = null;
				this.state.boss.moveCounter = 0;
				this.state.invaderCooldown = bossFireDelayFor(this.state.level, this.state.boss);
			}
		}
	}

	private updateScatterInvaders(): void {
		const remaining: ScatterInvader[] = [];
		for (const invader of this.state.scatterInvaders) {
			const next = {
				...invader,
				x: invader.x + invader.vx,
				y: invader.y + invader.vy,
			};
			if (next.x < -1 || next.x > GAME_WIDTH || next.y < -1 || next.y > GAME_HEIGHT) {
				continue;
			}
			remaining.push(next);
		}
		this.state.scatterInvaders = remaining;
	}

	private updateBoss(): void {
		if (!this.state.boss.active) return;
		const moveDelay = bossMoveDelayFor(this.state.boss, this.state.level);
		this.state.boss.moveCounter += 1;
		if (this.state.boss.moveCounter < moveDelay) return;
		this.state.boss.moveCounter = 0;
		let nextX = this.state.boss.x + this.state.boss.dir;
		if (nextX < 0 || nextX + BOSS_WIDTH > GAME_WIDTH) {
			this.state.boss.dir = (this.state.boss.dir === 1 ? -1 : 1) as Direction;
			nextX = this.state.boss.x + this.state.boss.dir;
		}
		this.state.boss.x = Math.max(0, Math.min(GAME_WIDTH - BOSS_WIDTH, nextX));
		this.state.boss.frame = this.state.boss.frame === 0 ? 1 : 0;
	}

	private applyHeldMovement(): void {
		if (this.state.playerMoveHold <= 0 || this.state.playerMoveDir === 0) return;
		this.state.playerX = Math.max(
			0,
			Math.min(GAME_WIDTH - 1, this.state.playerX + this.state.playerMoveDir * PLAYER_MOVE_STEP),
		);
		this.state.playerMoveHold -= 1;
	}

	private queuePlayerMove(dir: Direction): void {
		this.state.playerMoveDir = dir;
		this.state.playerMoveHold = PLAYER_MOVE_HOLD_TICKS;
		this.state.playerX = Math.max(0, Math.min(GAME_WIDTH - 1, this.state.playerX + dir * PLAYER_MOVE_STEP));
		this.markDirty();
	}

	private updateUfo(): void {
		if (this.state.ufo.active) {
			this.state.ufo.x += this.state.ufo.dir;
			if (this.state.ufo.x < 0 || this.state.ufo.x >= GAME_WIDTH) {
				this.state.ufo.active = false;
				this.state.ufoCooldown = UFO_BASE_COOLDOWN;
			}
			return;
		}

		if (this.state.ufoCooldown > 0) {
			this.state.ufoCooldown -= 1;
			return;
		}

		if (Math.random() < 0.25) {
			const dir: Direction = Math.random() < 0.5 ? 1 : -1;
			this.state.ufo = {
				x: dir === 1 ? 0 : GAME_WIDTH - 1,
				dir,
				active: true,
			};
		} else {
			this.state.ufoCooldown = 10;
		}
	}

	private moveBullets(): void {
		const nextPlayerBullets: Bullet[] = [];
		for (const bullet of this.state.playerBullets) {
			const moved = { ...bullet, y: bullet.y - 1 };
			if (moved.y >= 0) {
				nextPlayerBullets.push(moved);
			}
		}
		this.state.playerBullets = nextPlayerBullets;

		const invaderStep = this.state.boss.active
			? bossBulletStepFor(this.state.level, this.state.boss)
			: INVADER_BULLET_STEP_TICKS;
		const shouldMoveInvaderBullets = this.state.bulletTick % invaderStep === 0;
		const nextInvaderBullets: Bullet[] = [];
		for (const bullet of this.state.invaderBullets) {
			const moved = shouldMoveInvaderBullets ? { ...bullet, y: bullet.y + 1 } : { ...bullet };
			if (moved.y <= PLAYER_Y) {
				nextInvaderBullets.push(moved);
			}
		}
		this.state.invaderBullets = nextInvaderBullets;
	}

	private resolveBulletCollisions(): void {
		const invaderBulletByPos = new Map<string, Bullet>();
		for (const bullet of this.state.invaderBullets) {
			invaderBulletByPos.set(`${bullet.x},${bullet.y}`, bullet);
		}

		const invaderIndexByPos = new Map<string, number>();
		for (let i = 0; i < this.state.invaders.length; i++) {
			const invader = this.state.invaders[i];
			invaderIndexByPos.set(`${invader.x},${invader.y}`, i);
		}

		const bossBulletsUnblockable = this.state.boss.active && bossIsEnraged(this.state.boss);

		const hitBoss = (bullet: Bullet): boolean => {
			if (!this.state.boss.active) return false;
			const bossY = this.state.boss.y;
			if (bullet.y < bossY || bullet.y >= bossY + BOSS_HEIGHT) return false;
			if (bullet.x < this.state.boss.x || bullet.x >= this.state.boss.x + BOSS_WIDTH) return false;
			const wasEnraged = bossIsEnraged(this.state.boss);
			this.state.boss.hp -= 1;
			if (this.state.boss.hp <= 0) {
				this.state.boss.active = false;
				this.state.bossEnrageTicks = 0;
				this.state.bossEnrageBlink = false;
				this.state.score += BOSS_SCORE;
				if (this.state.score > this.state.highScore) {
					this.state.highScore = this.state.score;
				}
				return true;
			}
			if (!wasEnraged && bossIsEnraged(this.state.boss)) {
				this.state.bossEnrageTicks = BOSS_ENRAGE_TICKS;
				this.state.bossEnrageBlink = true;
			}
			return true;
		};

		const afterBulletClash: Bullet[] = [];
		for (const bullet of this.state.playerBullets) {
			const key = `${bullet.x},${bullet.y}`;
			const blocking = invaderBulletByPos.get(key);
			if (blocking) {
				if (!(blocking.unblockable || bossBulletsUnblockable)) {
					invaderBulletByPos.delete(key);
				}
				continue;
			}
			afterBulletClash.push(bullet);
		}
		this.state.invaderBullets = Array.from(invaderBulletByPos.values());

		const hitInvaders = new Map<number, number>();
		const remainingPlayerBullets: Bullet[] = [];
		for (const bullet of afterBulletClash) {
			const key = `${bullet.x},${bullet.y}`;
			if (this.state.ufo.active && bullet.y === UFO_Y && bullet.x === this.state.ufo.x) {
				this.state.ufo.active = false;
				this.state.ufoCooldown = UFO_BASE_COOLDOWN;
				this.state.score += UFO_SCORE;
				if (this.state.score > this.state.highScore) {
					this.state.highScore = this.state.score;
				}
				continue;
			}
			if (hitBoss(bullet)) {
				continue;
			}
			const invaderIndex = invaderIndexByPos.get(key);
			if (invaderIndex !== undefined) {
				if (!hitInvaders.has(invaderIndex)) {
					const invader = this.state.invaders[invaderIndex];
					hitInvaders.set(invaderIndex, invaderRowScoreFor(invader, this.state.invaderOffsetY));
				}
				continue;
			}
			remainingPlayerBullets.push(bullet);
		}

		if (hitInvaders.size > 0) {
			this.state.invaders = this.state.invaders.filter((_invader, idx) => !hitInvaders.has(idx));
			let scoreDelta = 0;
			for (const value of hitInvaders.values()) {
				scoreDelta += value;
			}
			this.state.score += scoreDelta;
			if (this.state.score > this.state.highScore) {
				this.state.highScore = this.state.score;
			}
			this.state.invaderMoveDelay = invaderDelayFor(this.state.level, this.state.invaders.length);
			const newFireDelay = invaderFireDelayFor(this.state.level, this.state.invaders.length);
			if (this.state.invaderCooldown > newFireDelay) {
				this.state.invaderCooldown = newFireDelay;
			}
		}
		this.state.playerBullets = remainingPlayerBullets;

		const remainingInvaderBullets: Bullet[] = [];
		for (const bullet of this.state.invaderBullets) {
			if (bullet.x === this.state.playerX && bullet.y === PLAYER_Y) {
				this.state.lives -= 1;
				if (this.state.lives <= 0) {
					this.state.gameOver = true;
				}
				continue;
			}
			remainingInvaderBullets.push(bullet);
		}
		this.state.invaderBullets = remainingInvaderBullets;
	}

	private moveInvaders(): void {
		this.state.invaderMoveCounter += 1;
		if (this.state.invaderMoveCounter < this.state.invaderMoveDelay) return;
		this.state.invaderMoveCounter = 0;

		const dir = this.state.invaderDir;
		let hitEdge = false;
		for (const invader of this.state.invaders) {
			const nextX = invader.x + dir;
			if (nextX < 0 || nextX >= GAME_WIDTH) {
				hitEdge = true;
				break;
			}
		}

		if (hitEdge) {
			this.state.invaderDir = (dir === 1 ? -1 : 1) as Direction;
			for (const invader of this.state.invaders) {
				invader.y += 1;
				if (invader.y >= PLAYER_Y) {
					this.state.gameOver = true;
				}
			}
			this.state.invaderOffsetY += 1;
		} else {
			for (const invader of this.state.invaders) {
				invader.x += dir;
			}
		}

		this.state.invaderFrame = this.state.invaderFrame === 0 ? 1 : 0;
	}

	private maybeFireInvaderBullet(): void {
		if (this.state.invaderCooldown > 0) return;
		const maxBullets = this.state.boss.active ? BOSS_MAX_BULLETS : MAX_INVADER_BULLETS;
		if (this.state.invaderBullets.length >= maxBullets) return;

		if (this.state.boss.active) {
			const fireChance = bossFireChanceFor(this.state.boss);
			if (Math.random() > fireChance) {
				this.state.invaderCooldown = 1;
				return;
			}
			const bossEnraged = bossIsEnraged(this.state.boss);
			const bulletY = this.state.boss.y + BOSS_HEIGHT;
			const leftX = this.state.boss.x + 1;
			const rightX = this.state.boss.x + BOSS_WIDTH - 2;
			const slots = maxBullets - this.state.invaderBullets.length;
			if (slots >= 1) {
				this.state.invaderBullets.push({
					x: leftX,
					y: bulletY,
					from: "invader",
					unblockable: bossEnraged,
				});
			}
			if (slots >= 2 && rightX !== leftX) {
				this.state.invaderBullets.push({
					x: rightX,
					y: bulletY,
					from: "invader",
					unblockable: bossEnraged,
				});
			}
			this.state.invaderCooldown = bossFireDelayFor(this.state.level, this.state.boss);
			return;
		}

		if (this.state.invaders.length === 0) return;

		const fireChance = invaderFireChanceFor(this.state.invaders.length);
		if (Math.random() > fireChance) {
			this.state.invaderCooldown = 1;
			return;
		}

		const shooters = new Map<number, Point>();
		for (const invader of this.state.invaders) {
			const existing = shooters.get(invader.x);
			if (!existing || invader.y > existing.y) {
				shooters.set(invader.x, invader);
			}
		}

		const shooterList = Array.from(shooters.values());
		if (shooterList.length === 0) return;
		const shooter = shooterList[Math.floor(Math.random() * shooterList.length)];

		this.state.invaderBullets.push({ x: shooter.x, y: shooter.y + 1, from: "invader" });
		this.state.invaderCooldown = invaderFireDelayFor(this.state.level, this.state.invaders.length);
	}

	private maybeAdvanceWave(): void {
		if (this.state.invaders.length > 0) return;
		if (this.state.pendingWave) return;
		this.state.pendingWave = true;
		this.state.wavePauseTicks = READY_TICKS;
		this.state.playerBullets = [];
		this.state.invaderBullets = [];
		this.state.playerMoveHold = 0;
		this.state.playerMoveDir = 0;
		this.state.ufo.active = false;
	}

	private startNextWave(): void {
		this.state.level += 1;
		this.state.invaders = createInvaders();
		this.state.invaderDir = 1;
		this.state.invaderFrame = 0;
		this.state.invaderMoveDelay = invaderDelayFor(this.state.level, this.state.invaders.length);
		this.state.invaderMoveCounter = 0;
		this.state.invaderOffsetY = 0;
		this.state.playerBullets = [];
		this.state.invaderBullets = [];
		this.state.playerCooldown = 0;
		this.state.invaderCooldown = invaderFireDelayFor(this.state.level, this.state.invaders.length);
		this.state.playerX = Math.floor(GAME_WIDTH / 2);
		this.state.playerMoveDir = 0;
		this.state.playerMoveHold = 0;
		this.state.ufo = { x: 0, dir: 1, active: false };
		this.state.ufoCooldown = UFO_BASE_COOLDOWN;
		this.state.wavePauseTicks = 0;
		this.state.pendingWave = false;
	}

	private togglePause(): void {
		this.paused = !this.paused;
		if (this.paused) {
			this.stopLoop();
		} else {
			this.startLoop();
		}
		this.markDirty();
	}

	private restartGame(): void {
		const highScore = this.state.highScore;
		this.state = createInitialState(highScore);
		this.paused = false;
		this.stopLoop();
		this.startLoop();
		this.onSave(null);
		this.markDirty();
	}

	handleInput(data: string): void {
		if (this.state.bossIntroPhase) {
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

		if (this.registerCheatInput(data)) {
			return;
		}

		if (this.paused) {
			if (matchesKey(data, "escape") || data === "q" || data === "Q") {
				this.dispose();
				this.onClose();
				return;
			}
			this.paused = false;
			this.startLoop();
			this.markDirty();
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

		if (matchesKey(data, "left") || data === "a" || data === "A") {
			this.queuePlayerMove(-1);
			return;
		}

		if (matchesKey(data, "right") || data === "d" || data === "D") {
			this.queuePlayerMove(1);
			return;
		}

		if (data === " " && !this.state.gameOver) {
			if (this.state.playerCooldown === 0 && this.state.playerBullets.length < MAX_PLAYER_BULLETS) {
				this.state.playerBullets.push({ x: this.state.playerX, y: PLAYER_Y - 1, from: "player" });
				this.state.playerCooldown = PLAYER_SHOT_DELAY;
				this.markDirty();
			}
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
		if (maxCells < MIN_RENDER_CELLS) {
			const message = "Lobster Invaders needs a wider terminal";
			const line = truncateToWidth(message, width);
			this.cachedLines = [line];
			this.cachedWidth = width;
			this.cachedVersion = this.version;
			return this.cachedLines;
		}

		const renderWidth = Math.min(GAME_WIDTH, maxCells);
		const boxWidth = renderWidth * CELL_WIDTH;

		const color = (code: string, text: string) => `\x1b[${code}m${text}\x1b[0m`;
		const dim = (text: string) => color("2", text);
		const accent = (text: string) => color("1;31", text);
		const scoreColor = (text: string) => color("33", text);
		const livesColor = (text: string) => color("32", text);
		const levelColor = (text: string) => color("35", text);
		const invaderColor = (text: string) => color("31", text);
		const bossEnraged = bossIsEnraged(this.state.boss);
		const bossCellColor = (text: string, col: number) => {
			if (!bossEnraged) return color("1;37", text);
			const center = (BOSS_WIDTH - 1) / 2;
			const dist = Math.abs(col - center);
			const code = dist <= 0.75 ? "1;31" : dist <= 1.75 ? "31" : "2;31";
			return color(code, text);
		};
		const bossBulletColor = (text: string) => color("95", text);
		const playerColor = (text: string) => color("1;36", text);
		const playerBulletColor = (text: string) => color("33", text);
		const invaderBulletColor = (text: string) => color("31;1", text);
		const ufoColor = (text: string) => color("1;37", text);

		const lines: string[] = [];
		const topBorder = dim(`+${"-".repeat(boxWidth)}+`);

		lines.push(this.padLine(topBorder, width));

		const titleLine = `${accent("LOBSTER INVADERS")}  ${dim(`Claws: ${this.state.invaders.length}`)}`;
		const statsLine =
			`Score: ${scoreColor(String(this.state.score))}  ` +
			`Lives: ${livesColor(String(this.state.lives))}  ` +
			`Level: ${levelColor(String(this.state.level))}  ` +
			`High: ${dim(String(this.state.highScore))}`;

		lines.push(this.padLine(this.boxLine(titleLine, boxWidth), width));
		lines.push(this.padLine(this.boxLine(statsLine, boxWidth), width));
		lines.push(this.padLine(this.boxLine(dim("-".repeat(boxWidth)), boxWidth), width));

		const invaderMap = new Set(this.state.invaders.map((invader) => `${invader.x},${invader.y}`));
		const scatterMap = new Set(this.state.scatterInvaders.map((invader) => `${invader.x},${invader.y}`));
		const invaderBulletMap = new Set(this.state.invaderBullets.map((bullet) => `${bullet.x},${bullet.y}`));
		const playerBulletMap = new Set(this.state.playerBullets.map((bullet) => `${bullet.x},${bullet.y}`));
		const bossMap = new Map<string, string>();
		const showBoss = this.state.boss.active || this.state.bossIntroPhase === "descend";
		if (showBoss) {
			const frame = BOSS_FRAMES[this.state.boss.frame];
			for (let row = 0; row < BOSS_HEIGHT; row++) {
				const y = this.state.boss.y + row;
				if (y < 0 || y >= GAME_HEIGHT) continue;
				for (let col = 0; col < BOSS_WIDTH; col++) {
					const cell = frame[row][col] ?? "  ";
					const key = `${this.state.boss.x + col},${y}`;
					bossMap.set(key, bossCellColor(cell, col));
				}
			}
		}

		const invaderGlyph = invaderColor(this.state.invaderFrame === 0 ? "}{" : "><");
		const playerGlyph = playerColor("/\\");
		const playerBulletGlyph = playerBulletColor("||");
		const invaderBulletGlyph = bossEnraged ? bossBulletColor("!!") : invaderBulletColor("!!");
		const ufoGlyph = ufoColor("==");

		for (let y = 0; y < GAME_HEIGHT; y++) {
			let row = "";
			for (let x = 0; x < renderWidth; x++) {
				const key = `${x},${y}`;
				if (this.state.ufo.active && y === UFO_Y && x === this.state.ufo.x) {
					row += ufoGlyph;
					continue;
				}
				if (playerBulletMap.has(key)) {
					row += playerBulletGlyph;
					continue;
				}
				if (invaderBulletMap.has(key)) {
					row += invaderBulletGlyph;
					continue;
				}
				const bossCell = bossMap.get(key);
				if (bossCell) {
					row += bossCell;
					continue;
				}
				if (y === PLAYER_Y && x === this.state.playerX) {
					row += playerGlyph;
					continue;
				}
				if (scatterMap.has(key)) {
					row += invaderGlyph;
					continue;
				}
				if (invaderMap.has(key)) {
					row += invaderGlyph;
					continue;
				}
				row += "  ";
			}
			lines.push(this.padLine(`|${row}|`, width));
		}

		lines.push(this.padLine(this.boxLine(dim("-".repeat(boxWidth)), boxWidth), width));

		let footer: string;
		if (this.state.bossEnrageTicks > 0) {
			footer = this.state.bossEnrageBlink ? accent("BOILING MAD LOBSTER") : "";
		} else if (this.state.pendingWave && this.state.wavePauseTicks > 0) {
			footer = `${accent("READY")} - Wave ${this.state.level + 1} incoming`;
		} else if (this.paused) {
			footer = `${accent("PAUSED")} - Press any key to resume, ${accent("Q")} to quit`;
		} else if (this.state.gameOver) {
			footer = `${color("31;1", "GAME OVER")} - Press ${accent("R")} to restart, ${accent("Q")} to quit`;
		} else {
			footer = `Left/Right or A/D move, ${accent("Space")} fire, ${accent("ESC")} save`;
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
	pi.registerCommand("spice-invaders", {
		description: "Play Lobster Invaders!",
		handler: async (_args, ctx) => {
			if (!ctx.hasUI) {
				ctx.ui.notify("Lobster Invaders requires interactive mode", "error");
				return;
			}

			const entries = ctx.sessionManager.getEntries();
			let savedState: GameState | undefined;
			for (let i = entries.length - 1; i >= 0; i--) {
				const entry = entries[i];
				if (entry.type === "custom" && entry.customType === SPACE_INVADERS_SAVE_TYPE) {
					savedState = entry.data as GameState;
					break;
				}
			}

			await ctx.ui.custom((tui, _theme, _kb, done) => {
				return new SpaceInvadersComponent(
					tui,
					() => done(undefined),
					(state) => {
						pi.appendEntry(SPACE_INVADERS_SAVE_TYPE, state);
					},
					savedState,
				);
			});
		},
	});
}
