// One Durable Object per project (`<owner>/<name>`): the records *about*
// history. Artifacts holds the history itself, one repo per tracker; this
// holds who is in the project, its branches — each a list of one commit per
// tracker in Git — which instances exist and which trackers they forked, and
// every checkpoint. A Durable Object handles one request at a time, so two
// agents writing at once queue rather than race, and a branch's list moves in
// one write: every tracker together, or none.

import { DurableObject } from "cloudflare:workers";

import { liveDeps } from "./index";
import type { MirrorInstance } from "./endpoint";
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
	createdBy: string;
	createdAt: string;
	updatedAt: string;
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
				old TEXT, new TEXT, moved_by TEXT NOT NULL, moved_at TEXT NOT NULL);
		`);
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
	 * list, once. Logged as moved by nobody, from nothing: what was there.
	 */
	private adoptLists(): void {
		const unadopted = this.sql
			.exec<BranchRow>(
				"SELECT * FROM branches WHERE name NOT IN (SELECT DISTINCT branch FROM heads)",
			)
			.toArray();
		if (unadopted.length === 0) return;
		this.ctx.storage.transactionSync(() => {
			for (const row of unadopted) {
				const list = JSON.parse(row.list) as TrackerList;
				this.moveHeads(row.name, {}, list, "", row.updated_at);
			}
		});
	}

	/**
	 * Set `name`'s heads from `from` to `to`, logging each tracker that moves,
	 * and keep the branch's list in step. Run inside a transaction.
	 *
	 * The list is written too, so a Worker from before heads, after a rollback,
	 * still reads the truth. It goes once no deploy can be rolled back past this.
	 */
	private moveHeads(name: string, from: TrackerList, to: TrackerList, by: string, at: string): void {
		for (const tracker of new Set([...Object.keys(from), ...Object.keys(to)])) {
			const old = from[tracker] ?? null;
			const now = to[tracker] ?? null;
			if (old === now) continue;
			if (now === null) this.sql.exec("DELETE FROM heads WHERE branch = ? AND tracker = ?", name, tracker);
			else this.sql.exec("INSERT OR REPLACE INTO heads VALUES (?, ?, ?)", name, tracker, now);
			this.sql.exec(
				"INSERT INTO head_log (branch, tracker, old, new, moved_by, moved_at) VALUES (?, ?, ?, ?, ?, ?)",
				name,
				tracker,
				old,
				now,
				by,
				at,
			);
		}
		this.sql.exec("UPDATE branches SET list = ?, updated_at = ? WHERE name = ?", JSON.stringify(to), at, name);
	}

	/** Each tracker's head on `name`: what a branch's list is now. */
	private headsOf(name: string): TrackerList {
		const list: TrackerList = {};
		for (const row of this.sql
			.exec<{ tracker: string; commit_id: string }>(
				"SELECT tracker, commit_id FROM heads WHERE branch = ? ORDER BY tracker",
				name,
			)
			.toArray())
			list[row.tracker] = row.commit_id;
		return list;
	}

	/** Every head that moved on `name` after `since` (0: all of them), oldest first. */
	headLog(name: string, since = 0): HeadMove[] {
		return this.sql
			.exec<HeadMoveRow>("SELECT * FROM head_log WHERE branch = ? AND seq > ? ORDER BY seq", name, since)
			.toArray()
			.map((row) => ({
				seq: row.seq,
				tracker: row.tracker,
				old: row.old,
				new: row.new,
				movedBy: row.moved_by,
				movedAt: row.moved_at,
			}));
	}

	/** Create the project. Returns false when it already exists. */
	init(name: string, owner: string): boolean {
		if (this.info() !== null) return false;
		// A deleted project's name is free again: start from nothing, even if
		// a request in flight during the delete wrote a row after it.
		this.destroy();
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
	destroy(): void {
		for (const table of ["meta", "members", "branches", "instances", "forks", "checkpoints"])
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
		if (this.branch(name) === null) return false;
		this.sql.exec("INSERT OR REPLACE INTO meta VALUES ('default_branch', ?)", name);
		return true;
	}

	branch(name: string): Branch | null {
		const row = this.sql.exec<BranchRow>("SELECT * FROM branches WHERE name = ?", name).toArray()[0];
		return row ? { ...toBranch(row), list: this.headsOf(name) } : null;
	}

	branches(): Branch[] {
		return this.sql
			.exec<BranchRow>("SELECT * FROM branches ORDER BY name")
			.toArray()
			.map((row) => ({ ...toBranch(row), list: this.headsOf(row.name) }));
	}

	/**
	 * Create a branch from a list. Returns null when it exists already: a
	 * branch's list moves only by merging (`newgit merge`), never by being
	 * written over.
	 */
	createBranch(name: string, list: TrackerList, by: string): Branch | null {
		if (this.branch(name) !== null) return null;
		const now = new Date().toISOString();
		this.ctx.storage.transactionSync(() => {
			this.sql.exec("INSERT INTO branches VALUES (?, ?, ?, ?, ?)", name, JSON.stringify(list), by, now, now);
			this.moveHeads(name, {}, list, by, now);
		});
		return this.branch(name);
	}

	/**
	 * Begin a merge into `name` that makes `target`: refused if its list is no
	 * longer `base` (someone else landed first: merge again). Nothing is
	 * locked: another merge may begin beside it, and whichever lands second
	 * finds the list moved. Merges begun more than `seconds` ago and never
	 * landed are forgotten here; their passwords come back to cancel.
	 */
	beginMerge(
		name: string,
		base: TrackerList,
		by: string,
		seconds: number,
		target: TrackerList,
		writes: string[] = [],
	): MergeStart {
		const branch = this.branch(name);
		if (branch === null) return { ok: false, reason: "missing" };
		if (!sameList(branch.list, base)) return { ok: false, reason: "moved", list: branch.list };
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
		const merge: PendingMerge = { by, target, writes, tokens: [], until };
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
	land(name: string, base: TrackerList, list: TrackerList, by: string): Landing {
		const branch = this.branch(name);
		if (branch === null) return { ok: false, reason: "missing" };
		const merge = this.pendingMerge(name, list);
		const done = { tokens: merge?.tokens ?? [], writes: merge?.writes ?? [] };
		// Kept as landed, its passwords handed back once to cancel.
		const landed = () => {
			if (merge !== null)
				this.sql.exec(
					"INSERT OR REPLACE INTO meta VALUES (?, ?)",
					pendingKey(name, list),
					JSON.stringify({ ...merge, tokens: [], landed: true }),
				);
		};
		if (sameList(branch.list, list)) {
			landed();
			return { ok: true, branch, ...done, already: true };
		}
		const moving = [...new Set([...Object.keys(base), ...Object.keys(list), "root"])].filter(
			(tracker) => tracker === "root" || base[tracker] !== list[tracker],
		);
		if (moving.some((tracker) => (branch.list[tracker] ?? null) !== (base[tracker] ?? null)))
			return { ok: false, reason: "moved", list: branch.list };
		this.ctx.storage.transactionSync(() => {
			this.moveHeads(name, branch.list, list, by, new Date().toISOString());
			landed();
		});
		return { ok: true, branch: this.branch(name)!, ...done, already: false };
	}

	/** `tracker`'s head on `name` now, or null. */
	head(name: string, tracker: string): string | null {
		return this.headsOf(name)[tracker] ?? null;
	}

	/** Whether `commit` has been `tracker`'s head on `name`, now or before. */
	wasHead(name: string, tracker: string, commit: string): boolean {
		if (this.headsOf(name)[tracker] === commit) return true;
		return (
			this.sql
				.exec(
					"SELECT 1 FROM head_log WHERE branch = ? AND tracker = ? AND (old = ? OR new = ?) LIMIT 1",
					name,
					tracker,
					commit,
					commit,
				)
				.toArray().length > 0
		);
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
	pullFailed(pull: MirrorPull): number {
		const attempts = (this.pulls().find((known) => samePull(known, pull))?.attempts ?? 0) + 1;
		this.writePulls(this.pulls().map((known) => (samePull(known, pull) ? { ...known, attempts } : known)));
		return attempts;
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
	beginPull(pull: MirrorPull): PullStart {
		const known = this.pulls().find((candidate) => samePull(candidate, pull));
		if (known?.base !== undefined) return { ok: true, base: known.base };
		const branch = this.branch(PULL_BRANCH);
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
		if (info === null || this.pulls().length === 0) return;
		const deps = liveDeps(this.env, (promise) => this.ctx.waitUntil(promise));
		if (await advancePulls(this.env, deps, { owner: info.owner, name: info.name }))
			await this.ctx.storage.setAlarm(Date.now() + PULL_POLL_MS);
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
export interface HeadMove {
	seq: number;
	tracker: string;
	old: string | null;
	new: string | null;
	/** Empty for heads adopted from a list made before heads. */
	movedBy: string;
	movedAt: string;
}

interface HeadMoveRow extends Record<string, SqlStorageValue> {
	seq: number;
	tracker: string;
	old: string | null;
	new: string | null;
	moved_by: string;
	moved_at: string;
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
