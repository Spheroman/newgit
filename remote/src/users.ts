// One Durable Object per GitHub login: the tokens that act as that user.
// A token is good only while its row here says so, so revoking one is a
// single row update and takes effect on the next request — no key rotation,
// nobody else logged out.

import { DurableObject } from "cloudflare:workers";

import type { TokenClaims, TokenKind } from "./auth";

export interface TokenRecord {
	id: string;
	kind: TokenKind;
	agent: string | null;
	project: string | null;
	createdAt: string;
	expiresAt: string;
	revokedAt: string | null;
	lastUsedAt: string | null;
}

/** last_used_at is written at most this often per token, so reads stay cheap. */
const LAST_USED_GRANULARITY_MS = 60_000;

export class UserDO extends DurableObject<Env> {
	private readonly sql: SqlStorage;

	constructor(ctx: DurableObjectState, env: Env) {
		super(ctx, env);
		this.sql = ctx.storage.sql;
		this.sql.exec(`
			CREATE TABLE IF NOT EXISTS tokens (
				id TEXT PRIMARY KEY, kind TEXT NOT NULL, agent TEXT, project TEXT,
				created_at TEXT NOT NULL, expires_at TEXT NOT NULL,
				revoked_at TEXT, last_used_at TEXT);
		`);
	}

	register(claims: TokenClaims): TokenRecord {
		this.sql.exec(
			"INSERT INTO tokens VALUES (?, ?, ?, ?, ?, ?, NULL, NULL)",
			claims.jti,
			claims.kind,
			claims.agent ?? null,
			claims.project ?? null,
			new Date(claims.iat * 1000).toISOString(),
			new Date(claims.exp * 1000).toISOString(),
		);
		return this.token(claims.jti)!;
	}

	/** Is this token recorded and not revoked? Records the use, coarsely. */
	check(id: string): boolean {
		const row = this.sql
			.exec<{ revoked_at: string | null; last_used_at: string | null }>(
				"SELECT revoked_at, last_used_at FROM tokens WHERE id = ?",
				id,
			)
			.toArray()[0];
		if (!row || row.revoked_at !== null) return false;
		const now = Date.now();
		if (row.last_used_at === null || now - Date.parse(row.last_used_at) > LAST_USED_GRANULARITY_MS)
			this.sql.exec("UPDATE tokens SET last_used_at = ? WHERE id = ?", new Date(now).toISOString(), id);
		return true;
	}

	token(id: string): TokenRecord | null {
		const row = this.sql.exec<TokenRow>("SELECT * FROM tokens WHERE id = ?", id).toArray()[0];
		return row ? toRecord(row) : null;
	}

	/** Every token not yet expired, revoked ones included (so a revocation is visible). */
	tokens(): TokenRecord[] {
		return this.sql
			.exec<TokenRow>("SELECT * FROM tokens WHERE expires_at > ? ORDER BY created_at", new Date().toISOString())
			.toArray()
			.map(toRecord);
	}

	/** Revoke one token. False when there is no such token. */
	revoke(id: string): boolean {
		return (
			this.sql.exec(
				"UPDATE tokens SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?",
				new Date().toISOString(),
				id,
			).rowsWritten > 0
		);
	}
}

interface TokenRow extends Record<string, SqlStorageValue> {
	id: string;
	kind: string;
	agent: string | null;
	project: string | null;
	created_at: string;
	expires_at: string;
	revoked_at: string | null;
	last_used_at: string | null;
}

function toRecord(row: TokenRow): TokenRecord {
	return {
		id: row.id,
		kind: row.kind as TokenKind,
		agent: row.agent,
		project: row.project,
		createdAt: row.created_at,
		expiresAt: row.expires_at,
		revokedAt: row.revoked_at,
		lastUsedAt: row.last_used_at,
	};
}
