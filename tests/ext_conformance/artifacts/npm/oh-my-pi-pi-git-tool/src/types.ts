export type Operation =
	| "status"
	| "diff"
	| "log"
	| "show"
	| "blame"
	| "branch"
	| "add"
	| "restore"
	| "commit"
	| "checkout"
	| "merge"
	| "rebase"
	| "stash"
	| "cherry-pick"
	| "fetch"
	| "pull"
	| "push"
	| "tag"
	| "pr"
	| "issue"
	| "ci"
	| "release";

export interface ToolResult<T> {
	data: T;
	_rendered: string;
}

export interface ToolConfirm {
	confirm: string;
	override: string;
	_rendered?: string;
}

export interface ToolError {
	error: string;
	code?: string;
	suggestion?: string;
	_rendered?: string;
}

export type ToolResponse<T> = ToolResult<T> | ToolConfirm | ToolError;

export interface FileStatus {
	path: string;
	status: "added" | "modified" | "deleted" | "renamed" | "copied";
	oldPath?: string;
}

export interface StatusParams {
	operation: "status";
	only?: "branch" | "modified" | "staged" | "untracked" | "conflicts" | "sync";
	ignored?: boolean;
}

export interface StatusResult {
	branch: string;
	upstream: string | null;
	ahead: number;
	behind: number;
	staged: FileStatus[];
	modified: FileStatus[];
	untracked: string[];
	conflicts: string[];
	ignored?: string[];
}

export type DiffTarget =
	| "unstaged"
	| "staged"
	| "head"
	| {
			from: string;
			to?: string;
	  };

export interface DiffParams {
	operation: "diff";
	target?: DiffTarget;
	paths?: string[];
	stat_only?: boolean;
	name_only?: boolean;
	context?: number;
	max_lines?: number;
	ignore_whitespace?: boolean;
}

export interface DiffLine {
	type: "context" | "add" | "delete";
	content: string;
	oldLineNo?: number;
	newLineNo?: number;
}

export interface Hunk {
	oldStart: number;
	oldCount: number;
	newStart: number;
	newCount: number;
	header: string;
	lines: DiffLine[];
}

export interface FileDiff {
	path: string;
	oldPath?: string;
	status: "added" | "modified" | "deleted" | "renamed" | "copied";
	binary: boolean;
	additions: number;
	deletions: number;
	hunks?: Hunk[];
}

export interface DiffResult {
	files: FileDiff[];
	stats: {
		filesChanged: number;
		insertions: number;
		deletions: number;
	};
	truncated: boolean;
	truncatedFiles?: string[];
}

export interface LogParams {
	operation: "log";
	limit?: number;
	ref?: string;
	author?: string;
	since?: string;
	until?: string;
	paths?: string[];
	grep?: string;
	format?: "oneline" | "short" | "full";
	stat?: boolean;
	merges?: boolean;
	first_parent?: boolean;
}

export interface CommitAuthor {
	name: string;
	email: string;
	date: string;
}

export interface Commit {
	sha: string;
	shortSha: string;
	author: CommitAuthor;
	committer: CommitAuthor;
	message: string;
	subject: string;
	parents: string[];
	stats?: { additions: number; deletions: number; files: number };
}

export interface LogResult {
	commits: Commit[];
	hasMore: boolean;
}

export interface ShowParams {
	operation: "show";
	ref: string;
	path?: string;
	diff?: boolean;
	stat?: boolean;
	lines?: { start: number; end: number };
}

export interface ShowCommitResult {
	commit: Commit;
	diff?: DiffResult;
}

export interface ShowFileResult {
	path: string;
	ref: string;
	content: string;
	truncated: boolean;
}

export interface BlameParams {
	operation: "blame";
	path: string;
	lines?: { start: number; end: number };
	root?: boolean;
	ignore_whitespace?: boolean;
}

export interface BlameLine {
	lineNo: number;
	sha: string;
	shortSha: string;
	author: string;
	date: string;
	content: string;
	original?: {
		sha: string;
		path: string;
		lineNo: number;
	};
}

export interface BlameResult {
	lines: BlameLine[];
}

export interface BranchParams {
	operation: "branch";
	action?: "list" | "create" | "delete" | "rename" | "current";
	name?: string;
	newName?: string;
	startPoint?: string;
	remotes?: boolean;
	force?: boolean;
}

export interface BranchInfo {
	name: string;
	sha: string;
	upstream?: string;
	ahead?: number;
	behind?: number;
	gone?: boolean;
}

export interface BranchListResult {
	current: string;
	local: BranchInfo[];
	remote?: BranchInfo[];
}

export interface AddParams {
	operation: "add";
	paths?: string[];
	update?: boolean;
	all?: boolean;
	dry_run?: boolean;
}

export interface AddResult {
	staged: string[];
}

export interface RestoreParams {
	operation: "restore";
	paths: string[];
	staged?: boolean;
	worktree?: boolean;
	source?: string;
}

export interface RestoreResult {
	restored: string[];
}

export interface CommitParams {
	operation: "commit";
	message: string;
	all?: boolean;
	allow_empty?: boolean;
	sign?: boolean;
	no_verify?: boolean;
	amend?: boolean;
}

export interface CommitResult {
	sha: string;
	shortSha: string;
	subject: string;
	stats: { additions: number; deletions: number; files: number };
}

export interface CheckoutParams {
	operation: "checkout";
	ref?: string;
	create?: boolean;
	paths?: string[];
	force?: boolean;
}

export interface CheckoutResult {
	branch?: string;
	previous?: string;
	restoredFiles?: string[];
}

export interface MergeParams {
	operation: "merge";
	ref: string;
	message?: string;
	no_ff?: boolean;
	ff_only?: boolean;
	squash?: boolean;
	abort?: boolean;
	continue?: boolean;
}

export interface MergeResult {
	status: "success" | "conflict" | "up-to-date" | "fast-forward";
	sha?: string;
	conflicts?: string[];
	mergedCommits?: number;
}

export interface RebaseParams {
	operation: "rebase";
	onto?: string;
	upstream?: string;
	abort?: boolean;
	continue?: boolean;
	skip?: boolean;
}

export interface RebaseResult {
	status: "success" | "conflict" | "up-to-date";
	conflicts?: string[];
	rebasedCommits?: number;
}

export interface StashParams {
	operation: "stash";
	action?: "push" | "pop" | "apply" | "drop" | "list" | "show";
	message?: string;
	include_untracked?: boolean;
	index?: number;
	keep_index?: boolean;
}

export interface StashEntry {
	index: number;
	message: string;
	branch: string;
	date: string;
}

export interface StashListResult {
	stashes: StashEntry[];
}

export interface StashShowResult {
	stats: { additions: number; deletions: number; files: number };
	files: string[];
}

export interface StashActionResult {
	status: "success";
}

export type StashResult = StashListResult | StashShowResult | StashActionResult;

export interface CherryPickParams {
	operation: "cherry-pick";
	commits: string[];
	no_commit?: boolean;
	abort?: boolean;
	continue?: boolean;
}

export interface CherryPickResult {
	status: "success" | "conflict";
	appliedCommits?: string[];
	conflicts?: string[];
}

export interface FetchParams {
	operation: "fetch";
	remote?: string;
	branch?: string;
	all?: boolean;
	prune?: boolean;
	tags?: boolean;
}

export interface FetchResult {
	updated: { ref: string; oldSha: string; newSha: string }[];
	pruned?: string[];
}

export interface PullParams {
	operation: "pull";
	remote?: string;
	branch?: string;
	rebase?: boolean;
	ff_only?: boolean;
}

export interface PullResult {
	status: "success" | "conflict" | "up-to-date";
	commits?: number;
	conflicts?: string[];
}

export interface PushParams {
	operation: "push";
	remote?: string;
	branch?: string;
	set_upstream?: boolean;
	tags?: boolean;
	force?: boolean;
	force_with_lease?: boolean;
	delete?: boolean;
	force_override?: boolean;
}

export interface PushResult {
	remote: string;
	branch: string;
	commits: number;
	newBranch: boolean;
}

export interface TagParams {
	operation: "tag";
	action?: "list" | "create" | "delete" | "push";
	name?: string;
	message?: string;
	ref?: string;
	force?: boolean;
	sign?: boolean;
}

export interface TagInfo {
	name: string;
	sha: string;
	message?: string;
	tagger?: { name: string; date: string };
	annotated: boolean;
}

export interface TagListResult {
	tags: TagInfo[];
}

export interface TagActionResult {
	status: "success";
}

export type TagResult = TagListResult | TagActionResult;

export interface PRParams {
	operation: "pr";
	action: "list" | "view" | "create" | "checkout" | "diff" | "merge" | "close" | "ready" | "review";
	number?: number;
	title?: string;
	body?: string;
	base?: string;
	head?: string;
	draft?: boolean;
	state?: "open" | "closed" | "merged" | "all";
	author?: string;
	limit?: number;
	merge_method?: "merge" | "squash" | "rebase";
	review_action?: "approve" | "request-changes" | "comment";
	review_body?: string;
}

export interface PRInfo {
	number: number;
	title: string;
	state: string;
	author: string;
	branch: string;
	base: string;
	url: string;
	createdAt: string;
	updatedAt: string;
	additions: number;
	deletions: number;
	commits: number;
	reviewDecision?: string;
	checks?: { passing: number; failing: number; pending: number };
}

export interface PRListResult {
	prs: PRInfo[];
}

export interface PRCreateResult {
	number: number;
	url: string;
}

export interface PRActionResult {
	status: "success";
	diff?: string;
}

export type PRResult = PRListResult | PRCreateResult | PRActionResult | { pr: PRInfo };

export interface IssueParams {
	operation: "issue";
	action: "list" | "view" | "create" | "close" | "reopen" | "comment";
	number?: number;
	title?: string;
	body?: string;
	state?: "open" | "closed" | "all";
	labels?: string[];
	assignee?: string;
	limit?: number;
	comment_body?: string;
}

export interface IssueInfo {
	number: number;
	title: string;
	state: string;
	author: string;
	body: string;
	labels: string[];
	assignees: string[];
	url: string;
	createdAt: string;
	comments: number;
}

export interface IssueListResult {
	issues: IssueInfo[];
}

export interface IssueCreateResult {
	number: number;
	url: string;
}

export interface IssueActionResult {
	status: "success";
}

export type IssueResult = IssueListResult | IssueCreateResult | IssueActionResult | { issue: IssueInfo };

export interface CIParams {
	operation: "ci";
	action: "list" | "view" | "watch" | "run" | "cancel" | "rerun";
	workflow?: string;
	run_id?: number;
	limit?: number;
	branch?: string;
	inputs?: Record<string, string>;
	logs_failed?: boolean;
}

export interface RunInfo {
	id: number;
	name: string;
	status: "queued" | "in_progress" | "completed";
	conclusion?: "success" | "failure" | "cancelled" | "skipped";
	branch: string;
	sha: string;
	url: string;
	createdAt: string;
	updatedAt: string;
}

export interface RunListResult {
	runs: RunInfo[];
}

export interface JobInfo {
	id: number;
	name: string;
	status: string;
	conclusion?: string;
	steps: { name: string; status: string; conclusion?: string }[];
}

export interface RunViewResult {
	run: RunInfo;
	jobs: JobInfo[];
	logs?: string;
}

export interface CIActionResult {
	status: "success";
}

export type CIResult = RunListResult | RunViewResult | CIActionResult;

export interface ReleaseParams {
	operation: "release";
	action: "list" | "view" | "create" | "delete" | "upload";
	tag?: string;
	title?: string;
	notes?: string;
	generate_notes?: boolean;
	draft?: boolean;
	prerelease?: boolean;
	target?: string;
	assets?: string[];
	limit?: number;
}

export interface ReleaseInfo {
	tag: string;
	name: string;
	body: string;
	draft: boolean;
	prerelease: boolean;
	createdAt: string;
	publishedAt: string;
	url: string;
	assets: { name: string; size: number; downloadCount: number }[];
}

export interface ReleaseListResult {
	releases: ReleaseInfo[];
}

export interface ReleaseActionResult {
	status: "success";
	url?: string;
}

export type ReleaseResult = ReleaseListResult | ReleaseInfo | ReleaseActionResult;

export type GitParams =
	| StatusParams
	| DiffParams
	| LogParams
	| ShowParams
	| BlameParams
	| BranchParams
	| AddParams
	| RestoreParams
	| CommitParams
	| CheckoutParams
	| MergeParams
	| RebaseParams
	| StashParams
	| CherryPickParams
	| FetchParams
	| PullParams
	| PushParams
	| TagParams
	| PRParams
	| IssueParams
	| CIParams
	| ReleaseParams;

export type SafetyLevel = "safe" | "warn" | "confirm" | "block";

export interface SafetyPolicy {
	forcePush: SafetyLevel;
	forcePushMain: SafetyLevel;
	hardReset: SafetyLevel;
	discardChanges: SafetyLevel;
	deleteBranch: SafetyLevel;
	amendPushed: SafetyLevel;
	rebasePushed: SafetyLevel;
}

export interface SafetyCheck {
	level: SafetyLevel;
	message: string;
	suggestion?: string;
	override?: string;
}

export interface SafetyResult {
	blocked: boolean;
	confirm: boolean;
	message?: string;
	suggestion?: string;
	override?: string;
	warnings: string[];
}
