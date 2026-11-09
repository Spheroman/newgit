// The slice of Artifacts the Worker uses, behind an interface: tests run
// under Miniflare, which has no Artifacts binding, and inject a fake.

export interface MintedToken {
	/** Artifacts' id for the token: what revoking it early names. */
	id: string;
	/** The Git password: the token without its `?expires=` suffix. */
	password: string;
	expiresAt: string;
}

import { type GitObject, packObjects } from "./objects";
import { parseAdvertisement, parseReport, type RefUpdate, receivePackBody, receivePackCommands } from "./refs";

/** What the Worker reads of a commit, straight from Artifacts. */
export interface CommitFacts {
	hash: string;
	tree: string;
	parents: string[];
	/** Committer time, in Unix seconds. */
	committedAt: number;
}

export interface TreeEntry {
	name: string;
	hash: string;
	type: "tree" | "blob" | "symlink" | "gitlink" | "exec";
}

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
	/** A commit's tree and parents, or null when the repo does not have it. */
	readCommit(name: string, hash: string): Promise<CommitFacts | null>;
	/** A tree's immediate entries, or null when the repo does not have it. */
	readTree(name: string, hash: string): Promise<TreeEntry[] | null>;
	/** The commit branch `branch` is at, or null when there is none. */
	branchHead(name: string, branch: string): Promise<string | null>;
	/**
	 * Move refs, each by compare-and-swap on its `old`, sending `objects`
	 * first when they are new to the repo. Per ref, whether it moved: one that did not held something else.
	 */
	updateRefs(name: string, updates: RefUpdate[], objects?: GitObject[]): Promise<Record<string, boolean>>;
	/**
	 * Push `pack`, a Git pack as a client sends one (thin is fine: the repo
	 * completes it from what it has), with `updates`, streamed through.
	 */
	sendPack(name: string, updates: RefUpdate[], pack: ReadableStream<Uint8Array>): Promise<Record<string, boolean>>;
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

	async readCommit(name: string, hash: string) {
		const known = readCommits.get(`${name} ${hash}`);
		if (known !== undefined) return known;
		try {
			using repo = await this.artifacts.get(name);
			const commit = await repo.readCommit(hash);
			if (commit === null) return null;
			const read = {
				hash: commit.hash,
				tree: commit.treeHash,
				parents: commit.parents,
				committedAt: commit.committedAt,
			};
			remember(readCommits, `${name} ${hash}`, read);
			return read;
		} catch (error) {
			if (isNotFound(error)) return null;
			throw error;
		}
	}

	async readTree(name: string, hash: string) {
		try {
			const known = readTrees.get(`${name} ${hash}`);
			if (known !== undefined) return known;
			using repo = await this.artifacts.get(name);
			const entries = await repo.readTree(hash);
			if (entries === null) return null;
			const read = entries.map(({ name, hash, type }) => ({ name, hash, type }));
			remember(readTrees, `${name} ${hash}`, read);
			return read;
		} catch (error) {
			if (isNotFound(error)) return null;
			throw error;
		}
	}

	async branchHead(name: string, branch: string) {
		try {
			using repo = await this.artifacts.get(name);
			const [head] = await repo.log({ ref: `refs/heads/${branch}`, limit: 1 });
			return head?.hash ?? null;
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

	async updateRefs(name: string, updates: RefUpdate[], objects?: GitObject[]) {
		if (updates.length === 0) return {};
		const pack = objects !== undefined && objects.length > 0 ? await packObjects(objects) : undefined;
		return this.withPassword(name, async (auth) => {
			const response = await fetch(`${this.url(name)}/git-receive-pack`, {
				method: "POST",
				headers: {
					authorization: auth,
					"user-agent": USER_AGENT,
					"content-type": "application/x-git-receive-pack-request",
					accept: "application/x-git-receive-pack-result",
				},
				body: await receivePackBody(updates, pack),
			});
			if (!response.ok) throw new Error(`${name}: moving refs answered ${response.status}`);
			return parseReport(new Uint8Array(await response.arrayBuffer()), updates);
		});
	}

	async sendPack(name: string, updates: RefUpdate[], pack: ReadableStream<Uint8Array>) {
		return this.withPassword(name, async (auth) => {
			// The commands, then the client's pack as it arrives: never held whole.
			const { readable, writable } = new TransformStream<Uint8Array, Uint8Array>();
			const writing = (async () => {
				const writer = writable.getWriter();
				await writer.write(receivePackCommands(updates));
				writer.releaseLock();
				await pack.pipeTo(writable);
			})();
			const response = await fetch(`${this.url(name)}/git-receive-pack`, {
				method: "POST",
				headers: {
					authorization: auth,
					"user-agent": USER_AGENT,
					"content-type": "application/x-git-receive-pack-request",
					accept: "application/x-git-receive-pack-result",
				},
				body: readable,
			});
			await writing;
			if (!response.ok) throw new Error(`${name}: the pack was answered ${response.status}`);
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

/**
 * Commits and trees read before, by repo and hash. An object never changes,
 * so what was read stays true; only an absence is asked again (it may have
 * been pushed since). Bounded: past this many, forgotten and read again.
 */
const readCommits = new Map<string, CommitFacts>();
const readTrees = new Map<string, TreeEntry[]>();
const OBJECTS_KEPT = 20_000;

function remember<T>(cache: Map<string, T>, key: string, value: T): void {
	if (cache.size >= OBJECTS_KEPT) cache.clear();
	cache.set(key, value);
}

/** Named, as Git names itself: a bare client can be turned away at the door. */
const USER_AGENT = "newgit-remote";
