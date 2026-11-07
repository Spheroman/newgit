// The slice of Artifacts the Worker uses, behind an interface: tests run
// under Miniflare, which has no Artifacts binding, and inject a fake.

export interface MintedToken {
	/** Artifacts' id for the token: what revoking it early names. */
	id: string;
	/** The Git password: the token without its `?expires=` suffix. */
	password: string;
	expiresAt: string;
}

import { parseAdvertisement, parseReport, type RefUpdate, receivePackBody } from "./refs";

export interface Repos {
	create(name: string): Promise<{ remote: string }>;
	/** The repo's Git remote, or null when it does not exist. */
	remote(name: string): Promise<string | null>;
	/** Where the repo is (or will be) served, without checking it exists. */
	url(name: string): string;
	delete(name: string): Promise<boolean>;
	/** Every repo whose name starts with `prefix`. */
	names(prefix: string): Promise<string[]>;
	mint(name: string, scope: "read" | "write", ttlSeconds: number): Promise<MintedToken>;
	/** Revoke a minted token before it expires. False when it no longer exists. */
	revokeToken(name: string, tokenId: string): Promise<boolean>;
	/** A file's text at `ref`, or null when the repo, ref, or path is missing. */
	readFile(name: string, ref: string, path: string): Promise<string | null>;
	/** Every ref the repo has, at its commit. */
	refs(name: string): Promise<Record<string, string>>;
	/**
	 * Move refs to commits the repo already has, each by compare-and-swap on
	 * its `old`. Per ref, whether it moved: one that did not held something else.
	 */
	updateRefs(name: string, updates: RefUpdate[]): Promise<Record<string, boolean>>;
}

/** Artifacts tokens are `art_v…_<secret>?expires=<unix>`; Git wants the part before `?`. */
export function tokenPassword(token: string): string {
	return token.split("?expires=")[0];
}

function isNotFound(error: unknown): boolean {
	return typeof error === "object" && error !== null && (error as { code?: string }).code === "NOT_FOUND";
}

export class ArtifactsRepos implements Repos {
	/** `gitBase` is `https://<account>.artifacts.cloudflare.net/git/<namespace>`. */
	constructor(
		private readonly artifacts: Artifacts,
		private readonly gitBase: string,
	) {}

	async create(name: string) {
		const created = await this.artifacts.create(name, { setDefaultBranch: "main" });
		return { remote: created.remote };
	}

	async remote(name: string) {
		try {
			using repo = await this.artifacts.get(name);
			await repo.info();
		} catch (error) {
			if (isNotFound(error)) return null;
			throw error;
		}
		return this.url(name);
	}

	url(name: string) {
		// info() carries no remote; the URL is fixed by account, namespace, and name.
		return `${this.gitBase}/${name}.git`;
	}

	async delete(name: string) {
		return this.artifacts.delete(name);
	}

	async names(prefix: string) {
		const names: string[] = [];
		let cursor: string | undefined;
		do {
			const page = await this.artifacts.list({ limit: 200, cursor });
			names.push(...page.repos.map((repo) => repo.name).filter((name) => name.startsWith(prefix)));
			cursor = page.cursor;
		} while (cursor);
		return names;
	}

	async mint(name: string, scope: "read" | "write", ttlSeconds: number) {
		using repo = await this.artifacts.get(name);
		const token = await repo.createToken(scope, ttlSeconds);
		return { id: token.id, password: tokenPassword(token.plaintext), expiresAt: token.expiresAt };
	}

	async revokeToken(name: string, tokenId: string) {
		try {
			using repo = await this.artifacts.get(name);
			return await repo.revokeToken(tokenId);
		} catch (error) {
			if (isNotFound(error)) return false;
			throw error;
		}
	}

	async readFile(name: string, ref: string, path: string) {
		try {
			using repo = await this.artifacts.get(name);
			// readFile does not resolve HEAD (seen against the live service);
			// the default branch is what HEAD means for a bare remote.
			const resolved = ref === "HEAD" ? (await repo.info()).defaultBranch : ref;
			const blob = await repo.readFile({ ref: resolved, path });
			return blob === null ? null : await blob.text();
		} catch (error) {
			if (isNotFound(error)) return null;
			throw error;
		}
	}

	async refs(name: string) {
		return this.withPassword(name, async (auth) => {
			const response = await fetch(`${this.url(name)}/info/refs?service=git-receive-pack`, {
				headers: { authorization: auth, "user-agent": USER_AGENT },
			});
			if (!response.ok) throw new Error(`${name}: listing refs answered ${response.status}`);
			return parseAdvertisement(new Uint8Array(await response.arrayBuffer()));
		});
	}

	async updateRefs(name: string, updates: RefUpdate[]) {
		if (updates.length === 0) return {};
		return this.withPassword(name, async (auth) => {
			const response = await fetch(`${this.url(name)}/git-receive-pack`, {
				method: "POST",
				headers: {
					authorization: auth,
					"user-agent": USER_AGENT,
					"content-type": "application/x-git-receive-pack-request",
					accept: "application/x-git-receive-pack-result",
				},
				body: await receivePackBody(updates),
			});
			if (!response.ok) throw new Error(`${name}: moving refs answered ${response.status}`);
			return parseReport(new Uint8Array(await response.arrayBuffer()), updates);
		});
	}

	/** Run `use` with a write password that lives a minute and is gone after. */
	private async withPassword<T>(name: string, use: (auth: string) => Promise<T>): Promise<T> {
		const token = await this.mint(name, "write", 60);
		try {
			return await use(`Basic ${btoa(`x:${token.password}`)}`);
		} finally {
			await this.revokeToken(name, token.id).catch(() => false);
		}
	}
}

/** Named, as Git names itself: a bare client can be turned away at the door. */
const USER_AGENT = "newgit-remote";
