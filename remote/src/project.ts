// One Durable Object per project (`<owner>/<name>`): the records *about*
// history. Artifacts holds the history itself, one repo per tracker; this
// holds who is in the project, its branches — each a list of one commit per
// code tracker — which instances exist and which trackers they forked, and
// every checkpoint. A Durable Object handles one request at a time, so two
// agents writing at once queue rather than race, and a branch's list moves in
// one write: every tracker together, or none.

import { DurableObject } from "cloudflare:workers";

export interface ProjectInfo {
	name: string;
	owner: string;
	createdAt: string;
	defaultBranch: string;
}

/** A branch: one commit per code tracker, by tracker name. */
export type TrackerList = Record<string, string>;

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
	/** The code trackers this instance has forked: the ones it changed. */
	forks: string[];
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
				created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS forks (
				instance TEXT NOT NULL, tracker TEXT NOT NULL, PRIMARY KEY (instance, tracker));
			CREATE TABLE IF NOT EXISTS checkpoints (
				instance TEXT NOT NULL, id TEXT NOT NULL, created_by TEXT NOT NULL, agent TEXT,
				created_at TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY (instance, id));
		`);
	}

	/** Create the project. Returns false when it already exists. */
	init(name: string, owner: string): boolean {
		if (this.info() !== null) return false;
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
		return row ? toBranch(row) : null;
	}

	branches(): Branch[] {
		return this.sql.exec<BranchRow>("SELECT * FROM branches ORDER BY name").toArray().map(toBranch);
	}

	/**
	 * Create a branch from a list. Returns null when it exists already: a
	 * branch's list moves only by merging (`newgit merge`), never by being
	 * written over.
	 */
	createBranch(name: string, list: TrackerList, by: string): Branch | null {
		if (this.branch(name) !== null) return null;
		const now = new Date().toISOString();
		this.sql.exec("INSERT INTO branches VALUES (?, ?, ?, ?, ?)", name, JSON.stringify(list), by, now, now);
		return this.branch(name);
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

	createInstance(name: string, owner: string, baseRev: string, agent: string | null = null): Instance | null {
		if (this.instance(name) !== null) return null;
		const now = new Date().toISOString();
		this.sql.exec(
			`INSERT INTO instances (name, owner, agent, base_rev, head_rev, created_at, updated_at)
			 VALUES (?, ?, ?, ?, NULL, ?, ?)`,
			name,
			owner,
			agent,
			baseRev,
			now,
			now,
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

	/** Record that `instance` forked `tracker`. */
	addFork(instance: string, tracker: string): void {
		this.sql.exec("INSERT OR IGNORE INTO forks VALUES (?, ?)", instance, tracker);
	}

	private toInstance(row: InstanceRow): Instance {
		const forks = this.sql
			.exec<{ tracker: string }>("SELECT tracker FROM forks WHERE instance = ? ORDER BY tracker", row.name)
			.toArray()
			.map((fork) => fork.tracker);
		return {
			name: row.name,
			owner: row.owner,
			agent: row.agent,
			baseRev: row.base_rev,
			headRev: row.head_rev,
			forks,
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

	checkpoints(instance?: string): Checkpoint[] {
		const rows =
			instance === undefined
				? this.sql.exec<CheckpointRow>("SELECT * FROM checkpoints ORDER BY created_at, instance, id")
				: this.sql.exec<CheckpointRow>(
						"SELECT * FROM checkpoints WHERE instance = ? ORDER BY created_at, id",
						instance,
					);
		return rows.toArray().map(toCheckpoint);
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
