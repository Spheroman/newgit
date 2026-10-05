// One Durable Object per project: the records *about* history. Artifacts holds
// the history itself; this holds who is in the project, which instances exist
// and what they are bound to, and every checkpoint. A Durable Object handles
// one request at a time, so two agents writing at once queue rather than race.

import { DurableObject } from "cloudflare:workers";

export interface ProjectInfo {
	name: string;
	owner: string;
	createdAt: string;
}

export type ForkState = "pending" | "ready" | "failed";

export interface Instance {
	name: string;
	owner: string;
	/** The agent that created it, acting as `owner`; null when the owner did. */
	agent: string | null;
	baseRev: string;
	headRev: string | null;
	forkState: ForkState;
	forkError: string | null;
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
			CREATE TABLE IF NOT EXISTS instances (
				name TEXT PRIMARY KEY, owner TEXT NOT NULL, base_rev TEXT NOT NULL, head_rev TEXT,
				fork_state TEXT NOT NULL, fork_error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS checkpoints (
				instance TEXT NOT NULL, id TEXT NOT NULL, created_by TEXT NOT NULL, created_at TEXT NOT NULL,
				record TEXT NOT NULL, PRIMARY KEY (instance, id));
		`);
		// Which agent (acting as its owner) made a record, when one did.
		for (const table of ["instances", "checkpoints"]) {
			const columns = this.sql.exec<{ name: string }>(`PRAGMA table_info(${table})`).toArray();
			if (!columns.some((column) => column.name === "agent"))
				this.sql.exec(`ALTER TABLE ${table} ADD COLUMN agent TEXT`);
		}
	}

	/** Create the project. Returns false when it already exists. */
	init(name: string, owner: string): boolean {
		if (this.info() !== null) return false;
		const now = new Date().toISOString();
		this.sql.exec("INSERT INTO meta VALUES ('name', ?), ('owner', ?), ('created_at', ?)", name, owner, now);
		this.sql.exec("INSERT INTO members VALUES (?, ?, ?)", owner, owner, now);
		return true;
	}

	info(): ProjectInfo | null {
		const rows = this.sql.exec<{ key: string; value: string }>("SELECT key, value FROM meta").toArray();
		const meta = Object.fromEntries(rows.map((row) => [row.key, row.value]));
		if (!meta.name) return null;
		return { name: meta.name, owner: meta.owner, createdAt: meta.created_at };
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
			`INSERT INTO instances (name, owner, base_rev, head_rev, fork_state, fork_error, created_at, updated_at, agent)
			 VALUES (?, ?, ?, NULL, 'pending', NULL, ?, ?, ?)`,
			name,
			owner,
			baseRev,
			now,
			now,
			agent,
		);
		return this.instance(name);
	}

	instance(name: string): Instance | null {
		const row = this.sql.exec<InstanceRow>("SELECT * FROM instances WHERE name = ?", name).toArray()[0];
		return row ? toInstance(row) : null;
	}

	instances(): Instance[] {
		return this.sql.exec<InstanceRow>("SELECT * FROM instances ORDER BY created_at, name").toArray().map(toInstance);
	}

	setForkState(name: string, state: ForkState, error: string | null = null): void {
		this.sql.exec(
			"UPDATE instances SET fork_state = ?, fork_error = ?, updated_at = ? WHERE name = ?",
			state,
			error,
			new Date().toISOString(),
			name,
		);
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

	/** Remove the instance and its checkpoints. Its repo is the caller's to delete. */
	deleteInstance(name: string): boolean {
		this.sql.exec("DELETE FROM checkpoints WHERE instance = ?", name);
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
	fork_state: string;
	fork_error: string | null;
	created_at: string;
	updated_at: string;
}

interface CheckpointRow extends Record<string, SqlStorageValue> {
	instance: string;
	id: string;
	created_by: string;
	agent: string | null;
	created_at: string;
	record: string;
}

function toInstance(row: InstanceRow): Instance {
	return {
		name: row.name,
		owner: row.owner,
		agent: row.agent,
		baseRev: row.base_rev,
		headRev: row.head_rev,
		forkState: row.fork_state as ForkState,
		forkError: row.fork_error,
		createdAt: row.created_at,
		updatedAt: row.updated_at,
	};
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
