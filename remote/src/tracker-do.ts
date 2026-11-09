// One Durable Object per tracker path (docs/design/anchoring.md, *Step 8:
// the remote by path*): the tracker's heads, one per branch, its log, an inbox
// of its children's moves (what it rolls up), an outbox of its own moves (to
// its parent's inbox), and the reservations a landing across several
// trackers takes. A tracker's history is its own, not a project's: nothing
// here waits on any other tracker, and moving a head is a compare-and-swap in
// this object alone.
//
// Each call names the tracker (`TrackerRef`): the object keeps it, to know
// its parent, and where to adopt heads from that an older remote kept in the
// project's `ProjectDO`.

import { DurableObject } from "cloudflare:workers";

import type { HeadMove } from "./project";

/** Which tracker an object is: its project (`owner/name`) and its tracker ID. */
export interface TrackerRef {
	project: string;
	tracker: string;
}

/** The object of `ref`'s tracker: named by its path, `owner/name/t3code/repos`. */
export function trackerPath(ref: TrackerRef): string {
	return ref.tracker === "root" ? ref.project : `${ref.project}/${ref.tracker.split("__").join("/")}`;
}

/** The tracker `id` is mounted in, by ID: `t3code__repos` in `t3code`, `t3code` in `root`. */
function parentId(id: string): string | null {
	if (id === "root") return null;
	const at = id.lastIndexOf("__");
	return at < 0 ? "root" : id.slice(0, at);
}

/** A move of a child, delivered to its parent's inbox. */
export interface ChildMove {
	branch: string;
	child: string;
	/** The child's own log sequence: with `child`, what makes a delivery once. */
	childSeq: number;
	old: string | null;
	new: string;
	by: string;
	at: string;
	/** The commit the change made in this parent, kept as its history when it may be. */
	via: string | null;
	/** The landing it was part of: one this parent took part in, it moved with. */
	token: string | null;
}

/** How long a reservation holds before anyone may abort it. */
const RESERVATION_MS = 60_000;

/** How soon an undelivered outbox entry is tried again. */
const DELIVERY_RETRY_MS = 5_000;

type Decision = "committed" | "aborted";

export class TrackerDO extends DurableObject<Env> {
	private readonly sql: SqlStorage;

	constructor(ctx: DurableObjectState, env: Env) {
		super(ctx, env);
		this.sql = ctx.storage.sql;
		this.sql.exec(`
			CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS heads (branch TEXT PRIMARY KEY, commit_id TEXT);
			CREATE TABLE IF NOT EXISTS log (
				seq INTEGER PRIMARY KEY AUTOINCREMENT, branch TEXT NOT NULL, old TEXT, new TEXT,
				moved_by TEXT NOT NULL, moved_at TEXT NOT NULL, via TEXT, token TEXT);
			CREATE TABLE IF NOT EXISTS inbox (
				seq INTEGER PRIMARY KEY AUTOINCREMENT, branch TEXT NOT NULL, child TEXT NOT NULL,
				child_seq INTEGER NOT NULL, old TEXT, new TEXT NOT NULL, moved_by TEXT NOT NULL,
				moved_at TEXT NOT NULL, via TEXT, token TEXT, UNIQUE (child, child_seq));
			CREATE TABLE IF NOT EXISTS rolled (branch TEXT PRIMARY KEY, seq INTEGER NOT NULL);
			CREATE TABLE IF NOT EXISTS reservations (
				token TEXT PRIMARY KEY, branch TEXT NOT NULL, base TEXT, next TEXT,
				anchor TEXT NOT NULL, until INTEGER NOT NULL, decision TEXT,
				moved_by TEXT NOT NULL, via TEXT);
			CREATE TABLE IF NOT EXISTS outbox (
				seq INTEGER PRIMARY KEY AUTOINCREMENT, entry TEXT NOT NULL);
		`);
	}

	// ── Who this is ──────────────────────────────────────────────────────

	private remember(ref: TrackerRef): void {
		this.sql.exec("INSERT OR IGNORE INTO meta VALUES ('project', ?), ('tracker', ?)", ref.project, ref.tracker);
	}

	private ref(): TrackerRef | null {
		const rows = this.sql.exec<{ key: string; value: string }>("SELECT key, value FROM meta").toArray();
		const meta = Object.fromEntries(rows.map((row) => [row.key, row.value]));
		return meta.project && meta.tracker ? { project: meta.project, tracker: meta.tracker } : null;
	}

	private stubOf(ref: TrackerRef): DurableObjectStub<TrackerDO> {
		return this.env.TRACKERS.getByName(trackerPath(ref));
	}

	// ── Heads ────────────────────────────────────────────────────────────

	/**
	 * A head an older remote kept in the project's `ProjectDO`: taken over the
	 * first time it is asked for here. Idempotent: a second ask finds it here.
	 */
	private async adopt(ref: TrackerRef, branch: string): Promise<void> {
		const known = this.sql.exec("SELECT 1 FROM heads WHERE branch = ?", branch).toArray().length > 0;
		if (known || this.sql.exec("SELECT 1 FROM meta WHERE key = ?", `adopted:${branch}`).toArray().length > 0) return;
		const handed = await this.env.PROJECTS.getByName(ref.project).handOff(ref.tracker, branch);
		this.sql.exec("INSERT OR IGNORE INTO meta VALUES (?, '1')", `adopted:${branch}`);
		if (handed === null) return;
		this.sql.exec("INSERT OR IGNORE INTO heads VALUES (?, ?)", branch, handed);
		// What it was handed is rolled up through everything before: the
		// project's own list pinned it.
		this.sql.exec("INSERT OR IGNORE INTO rolled VALUES (?, 0)", branch);
	}

	/** `branch`'s head now, or null. A reservation is resolved first: no read sees half a landing. */
	async head(ref: TrackerRef, branch: string): Promise<string | null> {
		this.remember(ref);
		await this.adopt(ref, branch);
		await this.settleReservations(branch);
		return this.storedHead(branch);
	}

	private storedHead(branch: string): string | null {
		return (
			this.sql.exec<{ commit_id: string | null }>("SELECT commit_id FROM heads WHERE branch = ?", branch).toArray()[0]
				?.commit_id ?? null
		);
	}

	/** Every branch this tracker has a head on, and the head. */
	async heads(ref: TrackerRef, branches: string[]): Promise<Record<string, string | null>> {
		const out: Record<string, string | null> = {};
		for (const branch of branches) out[branch] = await this.head(ref, branch);
		return out;
	}

	/**
	 * Move `branch`'s head by compare-and-swap: refused, changing nothing,
	 * unless it is `old` now and no landing has it reserved.
	 */
	async cas(
		ref: TrackerRef,
		branch: string,
		old: string | null,
		next: string | null,
		by: string,
		via: string | null = null,
	): Promise<boolean> {
		if ((await this.head(ref, branch)) !== old) return false;
		if (this.liveReservation(branch) !== null) return false;
		await this.write(ref, branch, old, next, by, via, null);
		return true;
	}

	private async write(
		ref: TrackerRef,
		branch: string,
		old: string | null,
		next: string | null,
		by: string,
		via: string | null,
		token: string | null,
	): Promise<void> {
		const at = new Date().toISOString();
		this.sql.exec("INSERT OR REPLACE INTO heads VALUES (?, ?)", branch, next);
		this.sql.exec(
			"INSERT INTO log (branch, old, new, moved_by, moved_at, via, token) VALUES (?, ?, ?, ?, ?, ?, ?)",
			branch,
			old,
			next,
			by,
			at,
			via,
			token,
		);
		const seq = this.sql.exec<{ seq: number }>("SELECT MAX(seq) AS seq FROM log").one().seq;
		// Everything in the inbox is pinned by the commit this tracker moved to
		// in a landing (its index was checked): nothing of it is left to roll up.
		if (token !== null || old === null) this.markRolled(branch);
		const parent = parentId(ref.tracker);
		// A first head (from nothing) is set by a landing at the parent or
		// above, whose commit pins it already: nothing for the parent to roll up.
		if (parent !== null && next !== null && old !== null) {
			const entry: ChildMove = {
				branch,
				child: ref.tracker,
				childSeq: seq,
				old,
				new: next,
				by,
				at,
				via,
				token,
			};
			this.sql.exec("INSERT INTO outbox (entry) VALUES (?)", JSON.stringify(entry));
			await this.deliver(ref);
		}
	}

	private markRolled(branch: string): void {
		const through = this.sql.exec<{ seq: number }>("SELECT COALESCE(MAX(seq), 0) AS seq FROM inbox").one().seq;
		this.sql.exec("INSERT OR REPLACE INTO rolled VALUES (?, ?)", branch, through);
	}

	/**
	 * The outbox to the parent's inbox: at least once (the parent drops a
	 * repeat), and again from the alarm while it does not arrive, so a parent
	 * never misses a move.
	 */
	private async deliver(ref: TrackerRef): Promise<void> {
		const parent = parentId(ref.tracker);
		if (parent === null) return;
		const rows = this.sql.exec<{ seq: number; entry: string }>("SELECT seq, entry FROM outbox ORDER BY seq").toArray();
		for (const row of rows) {
			try {
				await this.stubOf({ project: ref.project, tracker: parent }).receive(
					{ project: ref.project, tracker: parent },
					JSON.parse(row.entry) as ChildMove,
				);
				this.sql.exec("DELETE FROM outbox WHERE seq = ?", row.seq);
			} catch (error) {
				console.error(JSON.stringify({ outbox: trackerPath(ref), undelivered: String(error) }));
				await this.ctx.storage.setAlarm(Date.now() + DELIVERY_RETRY_MS);
				return;
			}
		}
	}

	async alarm(): Promise<void> {
		const ref = this.ref();
		if (ref !== null) await this.deliver(ref);
	}

	/** A child's move, into the inbox, once; the project hears it has something to roll up. */
	async receive(ref: TrackerRef, move: ChildMove): Promise<void> {
		this.remember(ref);
		await this.adopt(ref, move.branch);
		this.sql.exec(
			`INSERT OR IGNORE INTO inbox (branch, child, child_seq, old, new, moved_by, moved_at, via, token)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
			move.branch,
			move.child,
			move.childSeq,
			move.old,
			move.new,
			move.by,
			move.at,
			move.via,
			move.token,
		);
		if (this.pendingNow(move.branch).length > 0)
			await this.env.PROJECTS.getByName(ref.project).markPending(move.branch);
	}

	// ── Rolling up ───────────────────────────────────────────────────────

	/** Its children's moves on `branch` not rolled up yet, oldest first. */
	async pending(ref: TrackerRef, branch: string): Promise<HeadMove[]> {
		this.remember(ref);
		await this.adopt(ref, branch);
		return this.pendingNow(branch);
	}

	private pendingNow(branch: string): HeadMove[] {
		const through =
			this.sql.exec<{ seq: number }>("SELECT seq FROM rolled WHERE branch = ?", branch).toArray()[0]?.seq ?? 0;
		// A move made in a landing this tracker moved in too is pinned already.
		const mine = new Set(
			this.sql
				.exec<{ token: string }>("SELECT token FROM log WHERE branch = ? AND token IS NOT NULL", branch)
				.toArray()
				.map((row) => row.token),
		);
		return this.sql
			.exec<{
				seq: number;
				child: string;
				old: string | null;
				new: string;
				moved_by: string;
				moved_at: string;
				via: string | null;
				token: string | null;
			}>("SELECT * FROM inbox WHERE branch = ? AND seq > ? ORDER BY seq", branch, through)
			.toArray()
			.filter((row) => row.token === null || !mine.has(row.token))
			.map((row) => ({
				seq: row.seq,
				tracker: row.child,
				old: row.old,
				new: row.new,
				movedBy: row.moved_by,
				movedAt: row.moved_at,
				via: row.via,
			}));
	}

	/**
	 * A roll-up's one commit point: the head from `old` to `next` by
	 * compare-and-swap, and the inbox through `through` rolled up.
	 */
	async rolledUp(ref: TrackerRef, branch: string, old: string, next: string, through: number): Promise<boolean> {
		if ((await this.head(ref, branch)) !== old || this.liveReservation(branch) !== null) return false;
		if (next !== old) await this.write(ref, branch, old, next, "roll-up", null, null);
		this.sql.exec("INSERT OR REPLACE INTO rolled VALUES (?, ?)", branch, through);
		return true;
	}

	/** Whether `commit` has been `branch`'s head, now or before. */
	async wasHead(ref: TrackerRef, branch: string, commit: string): Promise<boolean> {
		if ((await this.head(ref, branch)) === commit) return true;
		return (
			this.sql
				.exec("SELECT 1 FROM log WHERE branch = ? AND (old = ? OR new = ?) LIMIT 1", branch, commit, commit)
				.toArray().length > 0
		);
	}

	/**
	 * Whether `commit` is in this tracker's history on any branch: a head it
	 * has been, or a change's own commit (`via`) a roll-up here kept.
	 */
	async known(ref: TrackerRef, commit: string): Promise<boolean> {
		this.remember(ref);
		return (
			this.sql.exec("SELECT 1 FROM heads WHERE commit_id = ? LIMIT 1", commit).toArray().length > 0 ||
			this.sql.exec("SELECT 1 FROM log WHERE old = ? OR new = ? LIMIT 1", commit, commit).toArray().length > 0 ||
			this.sql.exec("SELECT 1 FROM inbox WHERE via = ? LIMIT 1", commit).toArray().length > 0
		);
	}

	/** Its own moves on `branch`, oldest first. */
	async log(ref: TrackerRef, branch: string): Promise<HeadMove[]> {
		this.remember(ref);
		await this.adopt(ref, branch);
		return this.sql
			.exec<{
				seq: number;
				old: string | null;
				new: string | null;
				moved_by: string;
				moved_at: string;
				via: string | null;
			}>("SELECT * FROM log WHERE branch = ? ORDER BY seq", branch)
			.toArray()
			.map((row) => ({
				seq: row.seq,
				tracker: ref.tracker,
				old: row.old,
				new: row.new,
				movedBy: row.moved_by,
				movedAt: row.moved_at,
				via: row.via,
			}));
	}

	/** Forget everything: its project was deleted, and a new one by that name starts from nothing. */
	async forget(ref: TrackerRef): Promise<void> {
		void ref;
		for (const table of ["meta", "heads", "log", "inbox", "rolled", "reservations", "outbox"])
			this.sql.exec(`DELETE FROM ${table}`);
	}

	// ── A landing across several trackers ────────────────────────────────

	private liveReservation(branch: string): string | null {
		return (
			this.sql
				.exec<{ token: string }>(
					"SELECT token FROM reservations WHERE branch = ? AND decision IS NULL LIMIT 1",
					branch,
				)
				.toArray()[0]?.token ?? null
		);
	}

	/**
	 * Phase one: hold `branch` for landing `token`, if its head is `base`
	 * and nothing else holds it. `anchor` is the tracker that decides.
	 */
	async prepare(
		ref: TrackerRef,
		branch: string,
		token: string,
		base: string | null,
		next: string | null,
		anchor: string,
		by: string,
		via: string | null = null,
	): Promise<boolean> {
		if ((await this.head(ref, branch)) !== base) return false;
		const held = this.liveReservation(branch);
		if (held !== null && held !== token) return false;
		this.sql.exec(
			"INSERT OR REPLACE INTO reservations VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?)",
			token,
			branch,
			base,
			next,
			anchor,
			Date.now() + RESERVATION_MS,
			by,
			via,
		);
		return true;
	}

	/**
	 * The decision, kept by the anchor: the first to ask decides, the coordinator
	 * `committed` or a reader that met an expired reservation `aborted`.
	 * What it is now, either way.
	 */
	async decide(ref: TrackerRef, token: string, decision: Decision): Promise<Decision | null> {
		this.remember(ref);
		const known = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = ?", `decision:${token}`).toArray()[0];
		if (known) return known.value as Decision;
		this.sql.exec("INSERT INTO meta VALUES (?, ?)", `decision:${token}`, decision);
		return decision;
	}

	/** What the anchor decided for `token`, without deciding. */
	async decision(ref: TrackerRef, token: string): Promise<Decision | null> {
		this.remember(ref);
		const known = this.sql.exec<{ value: string }>("SELECT value FROM meta WHERE key = ?", `decision:${token}`).toArray()[0];
		return (known?.value as Decision | undefined) ?? null;
	}

	/** Phase two: make the move `token` reserved, once its anchor decided to. */
	async commit(ref: TrackerRef, token: string): Promise<boolean> {
		this.remember(ref);
		const row = this.sql
			.exec<{
				branch: string;
				base: string | null;
				next: string | null;
				decision: string | null;
				moved_by: string;
				via: string | null;
			}>("SELECT * FROM reservations WHERE token = ?", token)
			.toArray()[0];
		if (row === undefined) return false;
		if (row.decision === "committed") return true;
		if (row.decision !== null) return false;
		this.sql.exec("UPDATE reservations SET decision = 'committed' WHERE token = ?", token);
		await this.write(ref, row.branch, row.base, row.next, row.moved_by, row.via, token);
		return true;
	}

	/** Let go of `token`'s reservation, moving nothing. */
	async abort(ref: TrackerRef, token: string): Promise<void> {
		this.remember(ref);
		this.sql.exec("UPDATE reservations SET decision = 'aborted' WHERE token = ? AND decision IS NULL", token);
	}

	/**
	 * A reservation on `branch` a read meets: committed at its anchor, made
	 * here now; aborted there, or past its time and aborted by this read,
	 * let go. Undecided and in time: left, and the read sees the head before.
	 */
	private async settleReservations(branch: string): Promise<void> {
		const ref = this.ref();
		if (ref === null) return;
		for (const row of this.sql
			.exec<{ token: string; anchor: string; until: number }>(
				"SELECT token, anchor, until FROM reservations WHERE branch = ? AND decision IS NULL",
				branch,
			)
			.toArray()) {
			const anchorRef = { project: ref.project, tracker: row.anchor };
			const anchor = row.anchor === ref.tracker ? null : this.stubOf(anchorRef);
			let decided: Decision | null =
				anchor === null ? await this.decision(ref, row.token) : await anchor.decision(anchorRef, row.token);
			if (decided === null && Date.now() > row.until)
				decided =
					anchor === null
						? await this.decide(ref, row.token, "aborted")
						: await anchor.decide(anchorRef, row.token, "aborted");
			if (decided === "committed") {
				await this.commit(ref, row.token);
			} else if (decided === "aborted") {
				this.sql.exec("UPDATE reservations SET decision = 'aborted' WHERE token = ?", row.token);
			}
		}
	}
}
