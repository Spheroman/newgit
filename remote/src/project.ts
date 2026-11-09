// One Durable Object per project (`<owner>/<name>`): the records *about*
// history. Artifacts holds the history itself, one repo per tracker; this
// holds who is in the project, its branches — each a list of one commit per
// tracker in Git — which instances exist and which trackers they forked, and
// every checkpoint. A Durable Object handles one request at a time, so two
// agents writing at once queue rather than race, and a branch's list moves in
// one write: every tracker together, or none.

import { DurableObject } from "cloudflare:workers";

import { within } from "./anchor";
import { type TrackerDO, type TrackerRef, trackerPath } from "./tracker-do";
import { liveDeps } from "./index";
import type { MirrorInstance } from "./endpoint";
import { SWEEP_EVERY_MS, sweep } from "./sweep";
import { type MirrorPull, PULL_BRANCH, PULL_POLL_MS, PULL_START_MS, advancePulls } from "./webhook";

export interface ProjectInfo {
	name: string;
	owner: string;
	createdAt: string;
	defaultBranch: string;
}

/** A branch: one commit per tracker in Git, by tracker name. */
export type TrackerList = Record<string, string>;

/** Write passwords handed out, to cancel when what they were for ends. */
export type HandedToken = { repo: string; artifactsId: string };

/**
 * A merge begun and not landed: what was checked when it began, so its
 * landing lands exactly that. Many can be in flight at once; landing is the
 * only moment two of them meet.
 */
interface PendingMerge {
	by: string;
	/** The index it lands: every pin in it was checked when it began. */
	target: TrackerList;
	/** The trackers it was handed a write password for: the refs it may ask to move. */
	writes: string[];
	/**
	 * Where it lands: the lowest tracker covering every one it touched.
	 * Only that tracker and those inside it move; the trackers above roll up.
	 */
	anchor?: string;
	/**
	 * The anchor was worked out from the merge's own commits (sent before
	 * it began): the landing need not work it out again.
	 */
	read?: boolean;
	tokens: HandedToken[];
	/** Past this it is forgotten and its passwords cancelled. */
	until: string;
	/**
	 * It landed. Kept until `until`, so a finish asked again (one whose
	 * answer never arrived) can still follow, with the moves it was allowed.
	 */
	landed?: boolean;
}

export type MergeStart =
	| { ok: true; until: string; stale: HandedToken[] }
	| { ok: false; reason: "missing" }
	| { ok: false; reason: "moved"; list: TrackerList };

export type PullStart = { ok: true; base: TrackerList } | { ok: false; reason: "missing" };

export type Landing =
	| { ok: true; branch: Branch; tokens: HandedToken[]; writes: string[]; already: boolean }
	| { ok: false; reason: "missing" }
	| { ok: false; reason: "moved"; list: TrackerList };

function toMove(row: HeadMoveRow): HeadMove {
	return {
		seq: row.seq,
		tracker: row.tracker,
		old: row.old,
		new: row.new,
		movedBy: row.moved_by,
		movedAt: row.moved_at,
		via: row.via ?? null,
	};
}

/** `list`'s trackers at or inside `anchor`. */
function subtree(list: TrackerList, anchor: string): TrackerList {
	if (anchor === "root") return list;
	return Object.fromEntries(Object.entries(list).filter(([tracker]) => within(tracker, anchor)));
}

/** Where a pending merge is kept: by branch, and by the root commit it lands. */
function pendingKey(name: string, target: TrackerList): string {
	return `merge:${name}:${target.root ?? ""}`;
}

function samePull(left: MirrorPull, right: MirrorPull): boolean {
	return left.target === right.target && left.commit === right.commit;
}

function sameList(left: TrackerList, right: TrackerList): boolean {
	const keys = Object.keys(left).sort();
	const other = Object.keys(right).sort();
	return keys.length === other.length && keys.every((key, index) => key === other[index] && left[key] === right[key]);
}

export interface Branch {
	name: string;
	list: TrackerList;
	/**
	 * Its list is root's last commit, not every head: something landed
	 * that root could not roll up yet. Only on a branch served that way.
	 */
	behind?: boolean;
	createdBy: string;
	createdAt: string;
	updatedAt: string;
}

/**
 * A name for one point of the project's history: a frozen list, as a
 * checkpoint is, whose root commit is on root's `main`. Readers see it as
 * `refs/tags/<name>`, composed for what they may read.
 */
export interface Tag {
	name: string;
	list: TrackerList;
	/** An annotated tag's tagger and message; null for a lightweight one. */
	annotation: TagAnnotation | null;
	createdBy: string;
	createdAt: string;
}

/** What an annotated tag says besides where it points, as Git writes it. */
export interface TagAnnotation {
	/** The `tagger` line's value: `Name <email> <seconds> <zone>`. */
	tagger: string;
	message: string;
}

export interface Instance {
	name: string;
	owner: string;
	/** The agent that created it, acting as `owner`; null when the owner did. */
	agent: string | null;
	baseRev: string;
	headRev: string | null;
	/** The trackers in Git this instance has forked: the ones it changed. */
	forks: string[];
	/**
	 * Only its owner sees it — its record, its checkpoints, its forks —
	 * until its work is merged: an embargo, for work (a security fix) the
	 * rest of the project must not see yet.
	 */
	private: boolean;
	createdAt: string;
	updatedAt: string;
}

export interface Checkpoint {
	instance: string;
	id: string;
	createdBy: string;
	/** The agent that took it, acting as `createdBy`; null when a human did. */
	agent: string | null;
	createdAt: string;
	/** The CLI's `CheckpointRecord`, as it wrote it. The remote stores it, never interprets it. */
	record: unknown;
}

export class ProjectDO extends DurableObject<Env> {
	private readonly sql: SqlStorage;

	constructor(ctx: DurableObjectState, env: Env) {
		super(ctx, env);
		this.sql = ctx.storage.sql;
		this.sql.exec(`
			CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS members (
				login TEXT PRIMARY KEY, added_by TEXT NOT NULL, added_at TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS branches (
				name TEXT PRIMARY KEY, list TEXT NOT NULL, created_by TEXT NOT NULL,
				created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS instances (
				name TEXT PRIMARY KEY, owner TEXT NOT NULL, agent TEXT, base_rev TEXT NOT NULL, head_rev TEXT,
				created_at TEXT NOT NULL, updated_at TEXT NOT NULL, private INTEGER NOT NULL DEFAULT 0);
			CREATE TABLE IF NOT EXISTS forks (
				instance TEXT NOT NULL, tracker TEXT NOT NULL, complete INTEGER NOT NULL DEFAULT 1,
				PRIMARY KEY (instance, tracker));
			CREATE TABLE IF NOT EXISTS checkpoints (
				instance TEXT NOT NULL, id TEXT NOT NULL, created_by TEXT NOT NULL, agent TEXT,
				created_at TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY (instance, id));
			CREATE TABLE IF NOT EXISTS heads (
				branch TEXT NOT NULL, tracker TEXT NOT NULL, commit_id TEXT NOT NULL,
				PRIMARY KEY (branch, tracker));
			CREATE TABLE IF NOT EXISTS head_log (
				seq INTEGER PRIMARY KEY AUTOINCREMENT, branch TEXT NOT NULL, tracker TEXT NOT NULL,
				old TEXT, new TEXT, moved_by TEXT NOT NULL, moved_at TEXT NOT NULL, via TEXT);
			CREATE TABLE IF NOT EXISTS rollups (
				branch TEXT NOT NULL, tracker TEXT NOT NULL, seq INTEGER NOT NULL,
				PRIMARY KEY (branch, tracker));
			CREATE TABLE IF NOT EXISTS tags (
				name TEXT PRIMARY KEY, list TEXT NOT NULL, annotation TEXT,
				created_by TEXT NOT NULL, created_at TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS links (
				tracker TEXT PRIMARY KEY, home_project TEXT NOT NULL, home_tracker TEXT NOT NULL);
		`);
		// The log from before it named a change's commit in the parent.
		try {
			this.sql.exec("ALTER TABLE head_log ADD COLUMN via TEXT");
		} catch {}
		this.adoptLists();
		// Projects made before private instances: add the column (a no-op error otherwise).
		try {
			this.sql.exec("ALTER TABLE instances ADD COLUMN private INTEGER NOT NULL DEFAULT 0");
		} catch {}
		// Forks recorded before the remote copied them itself were recorded
		// only once Artifacts had made them: complete.
		try {
			this.sql.exec("ALTER TABLE forks ADD COLUMN complete INTEGER NOT NULL DEFAULT 1");
		} catch {}
	}

	/**
	 * Give every branch from before per-tracker heads its heads, from its
	 * list, once, in the legacy table a tracker's own object adopts from.
	 */
	private adoptLists(): void {
		const unadopted = this.sql
			.exec<BranchRow>("SELECT * FROM branches WHERE name NOT IN (SELECT DISTINCT branch FROM heads)")
			.toArray();
		for (const row of unadopted)
			for (const [tracker, commit] of Object.entries(JSON.parse(row.list) as TrackerList))
				if (commit) this.sql.exec("INSERT OR IGNORE INTO heads VALUES (?, ?, ?)", row.name, tracker, commit);
	}

	// ── Heads: each tracker's own object (src/tracker-do.ts) ─────────────

	/** This project's key, `owner/name`: the root tracker's path. */
	private key(): string {
		const info = this.info();
		return info === null ? "" : `${info.owner.toLowerCase()}/${info.name}`;
	}

	private refOf(tracker: string): TrackerRef {
		// A tracker linked from elsewhere is its home's: its object, its heads.
		const link = this.sql
			.exec<{ home_project: string; home_tracker: string }>("SELECT * FROM links WHERE tracker = ?", tracker)
			.toArray()[0];
		if (link !== undefined) return { project: link.home_project, tracker: link.home_tracker };
		const project = this.key();
		// Never an object named for no project: every nameless one would share it.
		if (project === "") throw new Error("this project does not exist: its trackers have no path");
		return { project, tracker };
	}

	/**
	 * What a branch here is called in `tracker`'s object. A linked tracker's
	 * object is its home's: `main` is its home's `main`, which every root
	 * linking it shares; any other branch is this project's own there,
	 * `<owner>/<name>:<branch>` (no Git branch has a `:`).
	 */
	private branchFor(tracker: string, name: string): string {
		if (!this.isLink(tracker) || name === "main") return name;
		return `${this.key()}:${name}`;
	}

	private isLink(tracker: string): boolean {
		return this.sql.exec("SELECT 1 FROM links WHERE tracker = ?", tracker).toArray().length > 0;
	}

	private trackerObject(tracker: string): DurableObjectStub<TrackerDO> {
		return this.env.TRACKERS.getByName(trackerPath(this.refOf(tracker)));
	}

	/**
	 * A head this object kept before trackers had objects of their own,
	 * handed to the tracker's object the first time it asks. It keeps no
	 * copy it would trust after.
	 */
	handOff(tracker: string, branch: string): string | null {
		return (
			this.sql
				.exec<{ commit_id: string }>("SELECT commit_id FROM heads WHERE branch = ? AND tracker = ?", branch, tracker)
				.toArray()[0]?.commit_id ?? null
		);
	}

	/**
	 * The trackers here that live elsewhere (`link`), and where: what the
	 * definitions say, as the Worker read them. Each is told it is linked
	 * from here, so its moves reach this project's parent of it too.
	 */
	/**
	 * The links the definitions on `main` name, for the trackers they
	 * define (`defined`): those become exactly these. A link recorded for a
	 * tracker not defined there yet (one a merge is adding) stays.
	 */
	async setLinks(links: Record<string, TrackerRef>, defined: string[]): Promise<void> {
		for (const tracker of defined)
			if (!(tracker in links)) this.sql.exec("DELETE FROM links WHERE tracker = ?", tracker);
		for (const [tracker, home] of Object.entries(links)) await this.addLink(tracker, home);
	}

	/** `tracker` here is `home` elsewhere: its history, heads and repo are there. */
	async addLink(tracker: string, home: TrackerRef): Promise<void> {
		const was = this.links()[tracker];
		this.sql.exec("INSERT OR REPLACE INTO links VALUES (?, ?, ?)", tracker, home.project, home.tracker);
		if (was?.project !== home.project || was?.tracker !== home.tracker)
			await this.env.TRACKERS.getByName(trackerPath(home)).addLinker(home, { project: this.key(), tracker });
	}

	/** The trackers here that live elsewhere, by what this project calls them. */
	links(): Record<string, TrackerRef> {
		return Object.fromEntries(
			this.sql
				.exec<{ tracker: string; home_project: string; home_tracker: string }>("SELECT * FROM links")
				.toArray()
				.map((row) => [row.tracker, { project: row.home_project, tracker: row.home_tracker }]),
		);
	}

	/** The trackers `name` lists: which objects to ask for its heads. */
	private trackersOf(name: string): string[] {
		const row = this.sql.exec<{ list: string }>("SELECT list FROM branches WHERE name = ?", name).toArray()[0];
		return row ? Object.keys(JSON.parse(row.list) as TrackerList) : [];
	}

	private writeTrackers(name: string, list: TrackerList): void {
		this.sql.exec(
			"UPDATE branches SET list = ?, updated_at = ? WHERE name = ?",
			JSON.stringify(list),
			new Date().toISOString(),
			name,
		);
	}

	/** Each tracker's head on `name`: what a branch's list is now. */
	private async headsOf(name: string): Promise<TrackerList> {
		const trackers = this.trackersOf(name);
		const heads = await Promise.all(
			trackers.map((tracker) => this.trackerObject(tracker).head(this.refOf(tracker), this.branchFor(tracker, name))),
		);
		const list: TrackerList = {};
		trackers.forEach((tracker, index) => {
			const head = heads[index];
			if (head !== null) list[tracker] = head;
		});
		return list;
	}

	/**
	 * Move one head by compare-and-swap, in its own object: refused, changing
	 * nothing, unless it is `old` now. `via` is the commit the change made in
	 * the tracker's parent, if any.
	 */
	async moveHead(
		name: string,
		tracker: string,
		old: string | null,
		next: string,
		by: string,
		via: string | null = null,
	): Promise<boolean> {
		const moved = await this.trackerObject(tracker).cas(this.refOf(tracker), this.branchFor(tracker, name), old, next, by, via);
		if (moved && !this.trackersOf(name).includes(tracker))
			this.writeTrackers(name, { ...(JSON.parse(this.listRow(name)) as TrackerList), [tracker]: next });
		return moved;
	}

	private listRow(name: string): string {
		return this.sql.exec<{ list: string }>("SELECT list FROM branches WHERE name = ?", name).toArray()[0]?.list ?? "{}";
	}

	/** A parent's object heard of a child's move: `name` has something to roll up. */
	markPending(name: string): void {
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES (?, '1')", `pending:${name}`);
	}

	/** What `parent` has not rolled up on `name`, oldest first. */
	async pending(name: string, parent: string): Promise<HeadMove[]> {
		return this.trackerObject(parent).pending(this.refOf(parent), this.branchFor(parent, name));
	}

	/** The branches with moves not rolled up yet, as their parents said. */
	pendingBranches(): string[] {
		return this.sql
			.exec<{ key: string }>("SELECT key FROM meta WHERE key LIKE 'pending:%'")
			.toArray()
			.map((row) => row.key.slice("pending:".length));
	}

	/** Whether any tracker on `name` has moves its parent has not rolled up; none forgets the mark. */
	async hasPending(name: string): Promise<boolean> {
		const parents = [...new Set(this.trackersOf(name).map(parentOf))].filter((parent) => parent !== null);
		const counts = await Promise.all(parents.map((parent) => this.pending(name, parent)));
		const any = counts.some((moves) => moves.length > 0);
		if (!any) this.sql.exec("DELETE FROM meta WHERE key = ?", `pending:${name}`);
		return any;
	}

	/** A roll-up's one commit point, in the parent's own object. */
	async rolledUp(name: string, parent: string, old: string, next: string, through: number): Promise<boolean> {
		return this.trackerObject(parent).rolledUp(this.refOf(parent), this.branchFor(parent, name), old, next, through);
	}

	/** Every head that moved on `name`, every tracker's, oldest first; `since` of them skipped. */
	async headLog(name: string, since = 0): Promise<HeadMove[]> {
		const logs = await Promise.all(
			this.trackersOf(name).map((tracker) => this.trackerObject(tracker).log(this.refOf(tracker), this.branchFor(tracker, name))),
		);
		return logs
			.flat()
			.sort((left, right) => left.movedAt.localeCompare(right.movedAt) || left.seq - right.seq)
			.slice(since);
	}

	/** Create the project. Returns false when it already exists. */
	init(name: string, owner: string): boolean {
		if (this.info() !== null) return false;
		// A deleted project's name is free again: start from nothing, even if
		// a request in flight during the delete wrote a row after it.
		this.clearRows();
		const now = new Date().toISOString();
		this.sql.exec(
			"INSERT INTO meta VALUES ('name', ?), ('owner', ?), ('created_at', ?), ('default_branch', 'main')",
			name,
			owner,
			now,
		);
		this.sql.exec("INSERT INTO members VALUES (?, ?, ?)", owner, owner, now);
		return true;
	}

	/**
	 * Forget the project: every row, so `info()` is null and the name can be
	 * created again. `meta` goes first, so the project is gone to every
	 * request from then on. The tables stay; this object keeps running on them.
	 */
	async destroy(): Promise<void> {
		// Its trackers' objects first, while their names are still known: a
		// project made again under this name starts from nothing.
		const trackers = new Set(
			this.sql
				.exec<{ list: string }>("SELECT list FROM branches")
				.toArray()
				.flatMap((row) => Object.keys(JSON.parse(row.list) as TrackerList)),
		);
		if (this.info() !== null)
			// A link's object is its home's, which outlives this project.
			await Promise.all(
				[...trackers]
					.filter((tracker) => !this.isLink(tracker))
					.map((tracker) => this.trackerObject(tracker).forget(this.refOf(tracker))),
			);
		this.clearRows();
	}

	private clearRows(): void {
		for (const table of [
			"meta",
			"members",
			"branches",
			"instances",
			"forks",
			"checkpoints",
			"heads",
			"head_log",
			"rollups",
			"tags",
			"links",
		])
			this.sql.exec(`DELETE FROM ${table}`);
	}

	info(): ProjectInfo | null {
		const rows = this.sql.exec<{ key: string; value: string }>("SELECT key, value FROM meta").toArray();
		const meta = Object.fromEntries(rows.map((row) => [row.key, row.value]));
		if (!meta.name) return null;
		return {
			name: meta.name,
			owner: meta.owner,
			createdAt: meta.created_at,
			defaultBranch: meta.default_branch ?? "main",
		};
	}

	setDefaultBranch(name: string): boolean {
		if (this.sql.exec("SELECT 1 FROM branches WHERE name = ?", name).toArray().length === 0) return false;
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES ('default_branch', ?)", name);
		return true;
	}

	async branch(name: string): Promise<Branch | null> {
		const row = this.sql.exec<BranchRow>("SELECT * FROM branches WHERE name = ?", name).toArray()[0];
		return row ? { ...toBranch(row), list: await this.headsOf(name) } : null;
	}

	async branches(): Promise<Branch[]> {
		const rows = this.sql.exec<BranchRow>("SELECT * FROM branches ORDER BY name").toArray();
		return Promise.all(rows.map(async (row) => ({ ...toBranch(row), list: await this.headsOf(row.name) })));
	}

	/**
	 * Create a branch from a list. Returns null when it exists already: a
	 * branch's list moves only by merging (`newgit merge`), never by being
	 * written over.
	 */
	async createBranch(name: string, list: TrackerList, by: string): Promise<Branch | null> {
		if (this.sql.exec("SELECT 1 FROM branches WHERE name = ?", name).toArray().length > 0) return null;
		const now = new Date().toISOString();
		this.sql.exec("INSERT INTO branches VALUES (?, ?, ?, ?, ?)", name, JSON.stringify(list), by, now, now);
		// Root last: its children's first moves reach its inbox before it has a
		// head, and are rolled up through when it gets one.
		const order = Object.keys(list).sort((left, right) => (left === "root" ? 1 : right === "root" ? -1 : 0));
		for (const tracker of order) {
			// A link's `main` is its home's, there already.
			if (this.isLink(tracker) && this.branchFor(tracker, name) === "main") continue;
			await this.trackerObject(tracker).cas(this.refOf(tracker), this.branchFor(tracker, name), null, list[tracker], by);
		}
		return this.branch(name);
	}

	/**
	 * Begin a merge into `name` that makes `target`: refused if its list is no
	 * longer `base` (someone else landed first: merge again). Nothing is
	 * locked: another merge may begin beside it, and whichever lands second
	 * finds the list moved. Merges begun more than `seconds` ago and never
	 * landed are forgotten here; their passwords come back to cancel.
	 */
	async beginMerge(
		name: string,
		base: TrackerList,
		by: string,
		seconds: number,
		target: TrackerList,
		writes: string[] = [],
		anchor = "root",
		read = false,
	): Promise<MergeStart> {
		const branch = await this.branch(name);
		if (branch === null) return { ok: false, reason: "missing" };
		// Every tracker it moves must still be where the merge saw it. Root is
		// left to the landing: only there is it known whether the merge
		// touches root, or lands below and lets root roll up meanwhile.
		const moves = [...new Set([...Object.keys(base), ...Object.keys(target)])].filter(
			(tracker) => tracker !== "root" && base[tracker] !== target[tracker],
		);
		if (moves.some((tracker) => (branch.list[tracker] ?? null) !== (base[tracker] ?? null)))
			return { ok: false, reason: "moved", list: branch.list };
		const stale: HandedToken[] = [];
		const now = Date.now();
		for (const [key, merge] of this.pendingMerges(name)) {
			if (Date.parse(merge.until) > now) continue;
			stale.push(...merge.tokens);
			this.sql.exec("DELETE FROM meta WHERE key = ?", key);
		}
		const until = new Date(now + seconds * 1000).toISOString();
		const key = pendingKey(name, target);
		// The same merge begun again (a run that stopped): its passwords from
		// before are cancelled, and it starts over.
		stale.push(...(this.pendingMerge(name, target)?.tokens ?? []));
		const merge: PendingMerge = { by, target, writes, tokens: [], until, anchor, read };
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES (?, ?)", key, JSON.stringify(merge));
		return { ok: true, until, stale };
	}

	/** The write passwords handed out for the merge into `name` that makes `target`. */
	recordMergeTokens(name: string, target: TrackerList, tokens: HandedToken[]): void {
		const merge = this.pendingMerge(name, target);
		if (merge === null) return;
		merge.tokens = [...merge.tokens, ...tokens];
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES (?, ?)", pendingKey(name, target), JSON.stringify(merge));
	}

	/** The merge into `name` that makes exactly `target`, begun and not landed. */
	pendingMerge(name: string, target: TrackerList): PendingMerge | null {
		const row = this.sql
			.exec<{ value: string }>("SELECT value FROM meta WHERE key = ?", pendingKey(name, target))
			.toArray()[0];
		const merge = row ? (JSON.parse(row.value) as PendingMerge) : null;
		return merge !== null && sameList(merge.target, target) ? merge : null;
	}

	private pendingMerges(name: string): [string, PendingMerge][] {
		return this.sql
			.exec<{ key: string; value: string }>(
				"SELECT key, value FROM meta WHERE substr(key, 1, ?) = ?",
				`merge:${name}:`.length,
				`merge:${name}:`,
			)
			.toArray()
			.map((row) => [row.key, JSON.parse(row.value) as PendingMerge]);
	}

	/**
	 * Land: move `name`'s list from `base` to `list` in one write. The commit
	 * point of every merge, and the only one: a compare-and-swap on the heads
	 * the merge moves, root's always among them, so a merge that began on a
	 * list someone else has since moved finds it moved. Already `list` (a run
	 * that landed and stopped before it heard): landed, and says so.
	 */
	async land(name: string, base: TrackerList, list: TrackerList, by: string, at?: string): Promise<Landing> {
		const branch = await this.branch(name);
		if (branch === null) return { ok: false, reason: "missing" };
		const merge = this.pendingMerge(name, list);
		const done = { tokens: merge?.tokens ?? [], writes: merge?.writes ?? [] };
		// Where it lands: `at`, worked out once its commits are there to read;
		// otherwise where it was seen to land when it began.
		const anchor = at ?? merge?.anchor ?? "root";
		// Kept as landed, its passwords handed back once to cancel.
		const landed = () => {
			if (merge !== null)
				this.sql.exec(
					"INSERT OR REPLACE INTO meta VALUES (?, ?)",
					pendingKey(name, list),
					JSON.stringify({ ...merge, tokens: [], landed: true, anchor }),
				);
		};
		if (merge?.landed === true || (merge === null && sameList(branch.list, list))) {
			landed();
			return { ok: true, branch, ...done, already: true };
		}
		// What it moves, and nothing else: at root, every tracker whose pin
		// changed and root itself; below, the anchor and what is inside it.
		// Only those are compared, and only those move.
		// A link joining or leaving this root moves nothing in its home: only
		// this root's pin of it, which its commit carries. Joining, it must be
		// at its home's head, or its pin is stale.
		const shared = (tracker: string) =>
			this.isLink(tracker) && this.branchFor(tracker, name) === "main" && !(tracker in base && tracker in list);
		for (const tracker of Object.keys(list))
			if (shared(tracker) && (await this.head(name, tracker)) !== list[tracker])
				return { ok: false, reason: "moved", list: branch.list };
		const moving = [...new Set([...Object.keys(base), ...Object.keys(list), anchor])]
			.filter((tracker) => within(tracker, anchor))
			.filter((tracker) => !shared(tracker))
			.filter((tracker) => tracker === anchor || base[tracker] !== list[tracker])
			.sort((left, right) => trackerPath(this.refOf(left)).localeCompare(trackerPath(this.refOf(right))));
		if (moving.some((tracker) => (branch.list[tracker] ?? null) !== (base[tracker] ?? null)))
			return { ok: false, reason: "moved", list: branch.list };
		const parent = parentOf(anchor);
		// A link's parent here is one of several: its commit is rolled up into
		// each, this one's own commit kept by none of them.
		const viaOf = (tracker: string) =>
			tracker === anchor && parent !== null && !this.isLink(tracker) ? (list[parent] ?? null) : null;
		const moved = await this.swap(name, anchor, moving, base, list, by, viaOf);
		if (!moved) return { ok: false, reason: "moved", list: (await this.branch(name))?.list ?? branch.list };
		// Which trackers the branch lists: a landing at root may add or drop one.
		if (anchor === "root") this.writeTrackers(name, list);
		landed();
		return { ok: true, branch: (await this.branch(name))!, ...done, already: false };
	}

	/**
	 * Move every tracker in `moving` from `base` to `list`, all or none. One
	 * tracker is one compare-and-swap in its object. Several are two-phase:
	 * each reserves in path order, the anchor's object decides, and each
	 * commits, shallowest first, so a parent has logged the landing before a
	 * child's move reaches its inbox (and is not taken for something left to
	 * roll up). A read that meets a reservation asks the anchor what it
	 * decided, so none sees half of it.
	 */
	private async swap(
		name: string,
		anchor: string,
		moving: string[],
		base: TrackerList,
		list: TrackerList,
		by: string,
		viaOf: (tracker: string) => string | null,
	): Promise<boolean> {
		const object = (tracker: string) => this.trackerObject(tracker);
		if (moving.length === 1) {
			const [tracker] = moving;
			return object(tracker).cas(this.refOf(tracker), this.branchFor(tracker, name), base[tracker] ?? null, list[tracker] ?? null, by, viaOf(tracker));
		}
		const token = crypto.randomUUID();
		const prepared: string[] = [];
		const abort = async () => {
			await Promise.all(prepared.map((tracker) => object(tracker).abort(this.refOf(tracker), token)));
			return false;
		};
		for (const tracker of moving) {
			const held = await object(tracker).prepare(
				this.refOf(tracker),
				this.branchFor(tracker, name),
				token,
				base[tracker] ?? null,
				list[tracker] ?? null,
				anchor,
				by,
				viaOf(tracker),
			);
			if (!held) return abort();
			prepared.push(tracker);
		}
		if ((await object(anchor).decide(this.refOf(anchor), token, "committed")) !== "committed") return abort();
		const shallowFirst = [...moving].sort((left, right) => left.split("__").length - right.split("__").length);
		for (const tracker of shallowFirst) await object(tracker).commit(this.refOf(tracker), token);
		return true;
	}

	/** `tracker`'s head on `name` now, or null. */
	async head(name: string, tracker: string): Promise<string | null> {
		return this.trackerObject(tracker).head(this.refOf(tracker), this.branchFor(tracker, name));
	}

	/** Whether `commit` has been `tracker`'s head on `name`, now or before. */
	async wasHead(name: string, tracker: string, commit: string): Promise<boolean> {
		return this.trackerObject(tracker).wasHead(this.refOf(tracker), this.branchFor(tracker, name), commit);
	}

	/** Webhook pulls waiting or running, oldest first (src/webhook.ts). */
	pulls(): MirrorPull[] {
		const row = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = 'pulls'").toArray()[0];
		return row ? (JSON.parse(row.value) as MirrorPull[]) : [];
	}

	private writePulls(pulls: MirrorPull[]) {
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES ('pulls', ?)", JSON.stringify(pulls));
	}

	/**
	 * Queue a pull and wake up to run it. A redelivery of one still queued
	 * changes nothing.
	 */
	async queuePull(pull: MirrorPull): Promise<void> {
		const pulls = this.pulls();
		if (!pulls.some((known) => samePull(known, pull))) this.writePulls([...pulls, pull]);
		await this.ctx.storage.setAlarm(Date.now() + PULL_START_MS);
	}

	/** Take a pull off the queue: the passwords its looks used, to cancel. */
	dropPull(pull: MirrorPull): { repo: string; artifactsId: string }[] {
		const tokens = this.pulls().find((known) => samePull(known, pull))?.tokens ?? [];
		this.writePulls(this.pulls().filter((known) => !samePull(known, pull)));
		return tokens;
	}

	/** A look found something to land. */
	markPeeked(pull: MirrorPull): void {
		this.writePulls(this.pulls().map((known) => (samePull(known, pull) ? { ...known, peeked: true } : known)));
	}

	/** Whose work this pull may hold: settled. */
	setPullInstances(pull: MirrorPull, instances: MirrorInstance[]): void {
		this.writePulls(this.pulls().map((known) => (samePull(known, pull) ? { ...known, instances } : known)));
	}

	/** GitHub listed no pull request: remember for how long, and when it was last asked. */
	noteLookup(pull: MirrorPull, lookup: NonNullable<MirrorPull["lookup"]>): void {
		this.writePulls(this.pulls().map((known) => (samePull(known, pull) ? { ...known, lookup } : known)));
	}

	notePullTokens(pull: MirrorPull, tokens: { repo: string; artifactsId: string }[]): void {
		this.writePulls(
			this.pulls().map((known) =>
				samePull(known, pull) ? { ...known, tokens: [...(known.tokens ?? []), ...tokens] } : known,
			),
		);
	}

	/**
	 * The instances with a fork of any of `trackers`, and what their last
	 * checkpoint holds of each: the one place the remote reads a checkpoint's
	 * record, because it has nowhere else to learn an instance's pins. A fork
	 * with no checkpoint says nothing, which reads as work: an empty pin.
	 */
	withheldWork(trackers: string[]): MirrorInstance[] {
		const work: MirrorInstance[] = [];
		for (const { name } of this.instances()) {
			const forked = this.forkRepos(name).filter((tracker) => trackers.includes(tracker));
			if (forked.length === 0) continue;
			const row = this.sql
				.exec<{ record: string }>(
					"SELECT record FROM checkpoints WHERE instance = ? ORDER BY created_at DESC, id DESC LIMIT 1",
					name,
				)
				.toArray()[0];
			const record = row ? (JSON.parse(row.record) as { code?: { head?: Record<string, string> } }) : undefined;
			work.push({ name, pins: Object.fromEntries(forked.map((tracker) => [tracker, record?.code?.head?.[tracker] ?? ""])) });
		}
		return work;
	}

	/** Count a failure. How many there have been. */
	pullFailed(pull: MirrorPull, why = ""): number {
		const attempts = (this.pulls().find((known) => samePull(known, pull))?.attempts ?? 0) + 1;
		this.writePulls(
			this.pulls().map((known) => (samePull(known, pull) ? { ...known, attempts, lastError: why } : known)),
		);
		return attempts;
	}

	/** How a webhook pull ended, kept (the last ten) for `newgit status` to say. */
	notePullEnded(ended: PullEnded): void {
		const row = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = 'pulls_ended'").toArray()[0];
		const kept = row ? (JSON.parse(row.value) as PullEnded[]) : [];
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES ('pulls_ended', ?)", JSON.stringify([...kept, ended].slice(-10)));
	}

	/** Whether rolling `name` up last worked; why not, when it did not. */
	noteRollup(name: string, why: string | null): void {
		if (why === null) this.sql.exec("DELETE FROM meta WHERE key = ?", `rollup_failed:${name}`);
		else
			this.sql.exec(
				"INSERT OR REPLACE INTO meta VALUES (?, ?)",
				`rollup_failed:${name}`,
				JSON.stringify({ at: new Date().toISOString(), why }),
			);
	}

	/**
	 * What the remote has not written down yet, read without rolling it up:
	 * per branch, how many moves each parent has pending and whether its
	 * last roll-up failed; the webhook pulls waiting, and how the last ones
	 * ended. What `newgit status` says, so "not written yet" never reads as
	 * "out of date". (Not `state`: Workers RPC does not expose a Durable
	 * Object method by that name, though the test runtime does.)
	 */
	async report(): Promise<ProjectState> {
		const ended = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = 'pulls_ended'").toArray()[0];
		const all = await this.branches();
		const counts = new Map<string, Record<string, number>>();
		for (const branch of all) {
			const pending: Record<string, number> = {};
			for (const tracker of new Set(Object.keys(branch.list).map(parentOf))) {
				if (tracker === null) continue;
				const count = (await this.pending(branch.name, tracker)).length;
				if (count > 0) pending[tracker] = count;
			}
			counts.set(branch.name, pending);
		}
		return {
			branches: all.map((branch) => {
				const pending = counts.get(branch.name) ?? {};
				const failed = this.sql
					.exec<{ value: string }>("SELECT value FROM meta WHERE key = ?", `rollup_failed:${branch.name}`)
					.toArray()[0];
				return {
					name: branch.name,
					pending,
					...(failed ? { rollupFailed: JSON.parse(failed.value) as { at: string; why: string } } : {}),
				};
			}),
			pulls: this.pulls().map((pull) => ({
				target: pull.target,
				commit: pull.commit,
				queuedAt: new Date(pull.queuedAt).toISOString(),
				attempts: pull.attempts,
				...(pull.lastError ? { lastError: pull.lastError } : {}),
			})),
			ended: ended ? (JSON.parse(ended.value) as PullEnded[]) : [],
		};
	}

	/** `main` moved under the pull: forget the index it began with. */
	restartPull(pull: MirrorPull): void {
		this.writePulls(this.pulls().map((known) => (samePull(known, pull) ? { ...known, base: undefined } : known)));
	}

	/**
	 * The index a pull lands on: `main`'s list when the pull began, kept for
	 * every step after. Nothing is locked; landing finds out whether `main`
	 * moved meanwhile.
	 */
	async beginPull(pull: MirrorPull): Promise<PullStart> {
		const known = this.pulls().find((candidate) => samePull(candidate, pull));
		if (known?.base !== undefined) return { ok: true, base: known.base };
		const branch = await this.branch(PULL_BRANCH);
		if (branch === null) return { ok: false, reason: "missing" };
		this.writePulls(
			this.pulls().map((candidate) => (samePull(candidate, pull) ? { ...candidate, base: branch.list } : candidate)),
		);
		return { ok: true, base: branch.list };
	}

	/**
	 * What keeps a pull going: nothing else polls the container once GitHub
	 * has had its 202. Stops when the queue is empty.
	 */
	async alarm(): Promise<void> {
		const info = this.info();
		if (info === null) return;
		const id = { owner: info.owner, name: info.name };
		const deps = liveDeps(this.env, (promise) => this.ctx.waitUntil(promise));
		const now = Date.now();
		let next = Number.POSITIVE_INFINITY;
		if (this.pulls().length > 0 && (await advancePulls(this.env, deps, id))) next = now + PULL_POLL_MS;
		const due = this.sweepAt();
		if (due !== null && due <= now) {
			this.sql.exec("DELETE FROM meta WHERE key = 'sweep_at'");
			// Still something to do (landing refs not yet old enough, a roll-up
			// that failed): again later.
			if (await sweep(this.env, deps, id)) this.writeSweepAt(now + SWEEP_EVERY_MS);
		}
		const sweepAt = this.sweepAt();
		if (sweepAt !== null) next = Math.min(next, sweepAt);
		if (Number.isFinite(next)) await this.ctx.storage.setAlarm(next);
	}

	/**
	 * Sweep within the hour, unless one is due sooner: after a landing or an
	 * upload, something may be left to roll up or to clear.
	 */
	async sweepSoon(): Promise<void> {
		if (this.sweepAt() === null) this.writeSweepAt(Date.now() + SWEEP_EVERY_MS);
		const at = this.sweepAt()!;
		const alarm = await this.ctx.storage.getAlarm();
		if (alarm === null || alarm > at) await this.ctx.storage.setAlarm(at);
	}

	private sweepAt(): number | null {
		const row = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = 'sweep_at'").toArray()[0];
		return row ? Number(row.value) : null;
	}

	private writeSweepAt(at: number): void {
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES ('sweep_at', ?)", String(at));
	}

	/**
	 * Whether `commit` is in `tracker`'s history as the remote knows it, on
	 * any branch: a head it has been, or a change's own commit a roll-up
	 * kept. A landing ref at one is spent.
	 */
	async knownCommit(tracker: string, commit: string): Promise<boolean> {
		return this.trackerObject(tracker).known(this.refOf(tracker), commit);
	}

	/** How many sweeps have seen each landing ref no head explains, by repo. */
	landingSightings(): Record<string, Record<string, number>> {
		const row = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = 'landing_sightings'").toArray()[0];
		return row ? (JSON.parse(row.value) as Record<string, Record<string, number>>) : {};
	}

	setLandingSightings(sightings: Record<string, Record<string, number>>): void {
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES ('landing_sightings', ?)", JSON.stringify(sightings));
	}

	isMember(login: string): boolean {
		return this.sql.exec("SELECT 1 FROM members WHERE login = ?", login).toArray().length > 0;
	}

	members(): { login: string; addedBy: string; addedAt: string }[] {
		return this.sql
			.exec<{ login: string; added_by: string; added_at: string }>(
				"SELECT login, added_by, added_at FROM members ORDER BY login",
			)
			.toArray()
			.map((row) => ({ login: row.login, addedBy: row.added_by, addedAt: row.added_at }));
	}

	addMember(login: string, by: string): boolean {
		if (this.isMember(login)) return false;
		this.sql.exec("INSERT INTO members VALUES (?, ?, ?)", login, by, new Date().toISOString());
		return true;
	}

	removeMember(login: string): boolean {
		return this.sql.exec("DELETE FROM members WHERE login = ?", login).rowsWritten > 0;
	}

	createInstance(
		name: string,
		owner: string,
		baseRev: string,
		agent: string | null = null,
		isPrivate = false,
	): Instance | null {
		if (this.instance(name) !== null) return null;
		const now = new Date().toISOString();
		this.sql.exec(
			`INSERT INTO instances (name, owner, agent, base_rev, head_rev, created_at, updated_at, private)
			 VALUES (?, ?, ?, ?, NULL, ?, ?, ?)`,
			name,
			owner,
			agent,
			baseRev,
			now,
			now,
			isPrivate ? 1 : 0,
		);
		return this.instance(name);
	}

	instance(name: string): Instance | null {
		const row = this.sql.exec<InstanceRow>("SELECT * FROM instances WHERE name = ?", name).toArray()[0];
		return row ? this.toInstance(row) : null;
	}

	instances(): Instance[] {
		return this.sql
			.exec<InstanceRow>("SELECT * FROM instances ORDER BY created_at, name")
			.toArray()
			.map((row) => this.toInstance(row));
	}

	/**
	 * Record that `instance`'s fork of `tracker` is being copied: its repo
	 * exists, but is not handed out until `completeFork`. A copy that died
	 * part way leaves it here, and the next ask copies again.
	 */
	startFork(instance: string, tracker: string): void {
		this.sql.exec("INSERT OR REPLACE INTO forks (instance, tracker, complete) VALUES (?, ?, 0)", instance, tracker);
	}

	/** Record that `instance`'s fork of `tracker` holds every branch of the tracker. */
	completeFork(instance: string, tracker: string): void {
		this.sql.exec("INSERT OR REPLACE INTO forks (instance, tracker, complete) VALUES (?, ?, 1)", instance, tracker);
	}

	/** Whether `instance`'s fork of `tracker` is complete. */
	forkComplete(instance: string, tracker: string): boolean {
		return (
			this.sql
				.exec("SELECT 1 FROM forks WHERE instance = ? AND tracker = ? AND complete = 1", instance, tracker)
				.toArray().length > 0
		);
	}

	/** Every tracker `instance` has a fork repo of, complete or not: what removing it deletes. */
	forkRepos(instance: string): string[] {
		return this.sql
			.exec<{ tracker: string }>("SELECT tracker FROM forks WHERE instance = ? ORDER BY tracker", instance)
			.toArray()
			.map((fork) => fork.tracker);
	}

	private toInstance(row: InstanceRow): Instance {
		const forks = this.sql
			.exec<{ tracker: string }>(
				"SELECT tracker FROM forks WHERE instance = ? AND complete = 1 ORDER BY tracker",
				row.name,
			)
			.toArray()
			.map((fork) => fork.tracker);
		return {
			name: row.name,
			owner: row.owner,
			agent: row.agent,
			baseRev: row.base_rev,
			headRev: row.head_rev,
			forks,
			private: row.private === 1,
			createdAt: row.created_at,
			updatedAt: row.updated_at,
		};
	}

	setHead(name: string, headRev: string): Instance | null {
		this.sql.exec(
			"UPDATE instances SET head_rev = ?, updated_at = ? WHERE name = ?",
			headRev,
			new Date().toISOString(),
			name,
		);
		return this.instance(name);
	}

	/** Remove the instance, its forks' records, and its checkpoints. Its fork repos are the caller's to delete. */
	deleteInstance(name: string): boolean {
		this.sql.exec("DELETE FROM checkpoints WHERE instance = ?", name);
		this.sql.exec("DELETE FROM forks WHERE instance = ?", name);
		return this.sql.exec("DELETE FROM instances WHERE name = ?", name).rowsWritten > 0;
	}

	tag(name: string): Tag | null {
		const row = this.sql.exec<TagRow>("SELECT * FROM tags WHERE name = ?", name).toArray()[0];
		return row ? toTag(row) : null;
	}

	tags(): Tag[] {
		return this.sql.exec<TagRow>("SELECT * FROM tags ORDER BY name").toArray().map(toTag);
	}

	/**
	 * Create a tag. A tag never moves: one that exists already is answered
	 * as it is, and the caller compares (the same tag again is a retry).
	 */
	createTag(name: string, list: TrackerList, annotation: TagAnnotation | null, by: string): { created: boolean; tag: Tag } {
		const existing = this.tag(name);
		if (existing !== null) return { created: false, tag: existing };
		const now = new Date().toISOString();
		this.sql.exec(
			"INSERT INTO tags VALUES (?, ?, ?, ?, ?)",
			name,
			JSON.stringify(list),
			annotation === null ? null : JSON.stringify(annotation),
			by,
			now,
		);
		return { created: true, tag: this.tag(name)! };
	}

	deleteTag(name: string): boolean {
		return this.sql.exec("DELETE FROM tags WHERE name = ?", name).rowsWritten > 0;
	}

	/**
	 * Record a checkpoint. Written last, after every ref it names was pushed:
	 * this row is what makes a checkpoint exist across machines. Rewriting
	 * the same id replaces it (a retried write-through is not a conflict).
	 */
	putCheckpoint(instance: string, id: string, by: string, agent: string | null, record: unknown): Checkpoint {
		const now = new Date().toISOString();
		this.sql.exec(
			`INSERT OR REPLACE INTO checkpoints (instance, id, created_by, created_at, record, agent)
			 VALUES (?, ?, ?, ?, ?, ?)`,
			instance,
			id,
			by,
			now,
			JSON.stringify(record),
			agent,
		);
		return { instance, id, createdBy: by, agent, createdAt: now, record };
	}

	checkpoint(instance: string, id: string): Checkpoint | null {
		const row = this.sql
			.exec<CheckpointRow>("SELECT * FROM checkpoints WHERE instance = ? AND id = ?", instance, id)
			.toArray()[0];
		return row ? toCheckpoint(row) : null;
	}

	/** Checkpoints `viewer` may see: none of another user's private instance. */
	checkpoints(instance: string | undefined, viewer: string): Checkpoint[] {
		const rows =
			instance === undefined
				? this.sql.exec<CheckpointRow>("SELECT * FROM checkpoints ORDER BY created_at, instance, id")
				: this.sql.exec<CheckpointRow>(
						"SELECT * FROM checkpoints WHERE instance = ? ORDER BY created_at, id",
						instance,
					);
		const hidden = new Set(
			this.sql
				.exec<{ name: string }>("SELECT name FROM instances WHERE private = 1 AND owner != ?", viewer)
				.toArray()
				.map((row) => row.name),
		);
		return rows
			.toArray()
			.map(toCheckpoint)
			.filter((checkpoint) => !hidden.has(checkpoint.instance));
	}
}

interface InstanceRow extends Record<string, SqlStorageValue> {
	name: string;
	owner: string;
	agent: string | null;
	base_rev: string;
	head_rev: string | null;
	created_at: string;
	updated_at: string;
	private: number;
}

/** One head moving: `old` null when the tracker joined, `new` null when it left. */
/** How a webhook pull ended. */
export interface PullEnded {
	target: string;
	commit: string;
	outcome: "landed" | "nothing" | "refused" | "gave up";
	why?: string;
	at: string;
}

/** `ProjectDO.state`: what is not written down yet, and the webhook's pulls. */
export interface ProjectState {
	branches: { name: string; pending: Record<string, number>; rollupFailed?: { at: string; why: string } }[];
	pulls: { target: string; commit: string; queuedAt: string; attempts: number; lastError?: string }[];
	ended: PullEnded[];
}

export interface HeadMove {
	seq: number;
	tracker: string;
	old: string | null;
	new: string | null;
	/** Empty for heads adopted from a list made before heads. */
	movedBy: string;
	movedAt: string;
	/**
	 * The commit the change that moved this head made in the tracker's
	 * parent, when it made one: what rolling up keeps as its own entry in
	 * the parent's history.
	 */
	via: string | null;
}

interface HeadMoveRow extends Record<string, SqlStorageValue> {
	seq: number;
	tracker: string;
	old: string | null;
	new: string | null;
	moved_by: string;
	moved_at: string;
	via: string | null;
}

/** The tracker `id` is mounted in: `t3code__repos` in `t3code`, `t3code` in `root`. */
export function parentOf(id: string): string | null {
	if (id === "root") return null;
	const at = id.lastIndexOf("__");
	return at < 0 ? "root" : id.slice(0, at);
}

interface BranchRow extends Record<string, SqlStorageValue> {
	name: string;
	list: string;
	created_by: string;
	created_at: string;
	updated_at: string;
}

function toBranch(row: BranchRow): Branch {
	return {
		name: row.name,
		list: JSON.parse(row.list) as TrackerList,
		createdBy: row.created_by,
		createdAt: row.created_at,
		updatedAt: row.updated_at,
	};
}

interface TagRow extends Record<string, SqlStorageValue> {
	name: string;
	list: string;
	annotation: string | null;
	created_by: string;
	created_at: string;
}

function toTag(row: TagRow): Tag {
	return {
		name: row.name,
		list: JSON.parse(row.list) as TrackerList,
		annotation: row.annotation === null ? null : (JSON.parse(row.annotation) as TagAnnotation),
		createdBy: row.created_by,
		createdAt: row.created_at,
	};
}

interface CheckpointRow extends Record<string, SqlStorageValue> {
	instance: string;
	id: string;
	created_by: string;
	agent: string | null;
	created_at: string;
	record: string;
}

function toCheckpoint(row: CheckpointRow): Checkpoint {
	return {
		instance: row.instance,
		id: row.id,
		createdBy: row.created_by,
		agent: row.agent,
		createdAt: row.created_at,
		record: JSON.parse(row.record),
	};
}
