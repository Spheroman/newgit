import type { CommitFacts, MintedToken, Repos, TreeEntry } from "../src/artifacts";
import type { GitObject } from "../src/objects";
import type { RefUpdate } from "../src/refs";
import type { CopyAnswer, CopyPlan } from "../src/endpoint";

/** The password the fake mints for a repo and scope: recognisable, not random. */
export const fakePassword = (name: string, scope: string) => ["pw", name, scope].join("-");

/** Artifacts in memory: repos are file maps; tokens are recognisable strings. */
export class FakeRepos implements Repos {
	readonly repos = new Map<string, Map<string, string>>();
	readonly minted: { name: string; scope: string; id: string }[] = [];
	readonly revokedTokens = new Set<string>();
	/** Each repo's refs, by name, as Git would hold them. */
	readonly refsOf = new Map<string, Map<string, string>>();
	/** Every ref update asked for, in order, and whether it held. */
	readonly updates: { name: string; update: RefUpdate; moved: boolean }[] = [];

	async create(name: string) {
		if (this.repos.has(name)) throw Object.assign(new Error("exists"), { code: "ALREADY_EXISTS" });
		this.repos.set(name, new Map());
		return { remote: this.url(name) };
	}
	async remote(name: string) {
		return this.repos.has(name) ? this.url(name) : null;
	}
	async delete(name: string) {
		return this.repos.delete(name);
	}
	async names(prefix: string) {
		return [...this.repos.keys()].filter((name) => name.startsWith(prefix));
	}
	async mint(name: string, scope: "read" | "write"): Promise<MintedToken> {
		if (!this.repos.has(name)) throw Object.assign(new Error(`${name} not found`), { code: "NOT_FOUND" });
		const id = `art-${this.minted.length + 1}`;
		this.minted.push({ name, scope, id });
		return { id, password: fakePassword(name, scope), expiresAt: "2099-01-01T00:00:00Z" };
	}
	async revokeToken(_name: string, tokenId: string) {
		this.revokedTokens.add(tokenId);
		return true;
	}
	/** How many files the Worker read. */
	fileReads = 0;
	async readFile(name: string, _ref: string, path: string) {
		this.fileReads++;
		return this.repos.get(name)?.get(path) ?? null;
	}

	async refs(name: string) {
		return Object.fromEntries(this.refsOf.get(name) ?? []);
	}
	/** Every object pushed, by repo. */
	readonly pushed: { name: string; objects: GitObject[] }[] = [];
	async updateRefs(name: string, updates: RefUpdate[], objects: GitObject[] = []) {
		if (objects.length > 0) this.pushed.push({ name, objects });
		for (const object of objects) {
			if (object.type === "tree") this.addTree(name, object.hash, parseTree(object.body));
			else this.addCommit(name, parseCommit(object.hash, object.body));
		}
		const refs = this.refsOf.get(name) ?? new Map<string, string>();
		this.refsOf.set(name, refs);
		const moved: Record<string, boolean> = {};
		for (const update of updates) {
			moved[update.ref] = (refs.get(update.ref) ?? null) === update.old;
			if (moved[update.ref]) {
				if (update.new === null) refs.delete(update.ref);
				else refs.set(update.ref, update.new);
			}
			this.updates.push({ name, update, moved: moved[update.ref] });
		}
		return moved;
	}

	/** Commits and trees by repo and hash, for the Worker's own reads. Empty: it asks Git. */
	readonly commits = new Map<string, Map<string, CommitFacts>>();
	readonly trees = new Map<string, Map<string, TreeEntry[]>>();
	/** How many object reads the Worker made. */
	reads = 0;
	async readCommit(name: string, hash: string) {
		this.reads++;
		return this.commits.get(name)?.get(hash) ?? null;
	}
	async readTree(name: string, hash: string) {
		this.reads++;
		return this.trees.get(name)?.get(hash) ?? null;
	}
	async branchHead(name: string, branch: string) {
		return this.refsOf.get(name)?.get(`refs/heads/${branch}`) ?? null;
	}
	addCommit(name: string, commit: CommitFacts) {
		const commits = this.commits.get(name) ?? new Map<string, CommitFacts>();
		this.commits.set(name, commits);
		commits.set(commit.hash, commit);
	}
	addTree(name: string, hash: string, entries: TreeEntry[]) {
		const trees = this.trees.get(name) ?? new Map<string, TreeEntry[]>();
		this.trees.set(name, trees);
		trees.set(hash, entries);
	}

	/** Packs sent through, by repo: what a client uploaded before a merge. */
	readonly packs: { name: string; updates: RefUpdate[]; bytes: number }[] = [];
	/**
	 * A pack sent through. The fake cannot read a pack: a test adds the
	 * objects it holds with `addCommit`/`addTree`, and the refs move.
	 */
	async sendPack(name: string, updates: RefUpdate[], pack: ReadableStream<Uint8Array>) {
		const bytes = (await new Response(pack).arrayBuffer()).byteLength;
		this.packs.push({ name, updates, bytes });
		return this.updateRefs(name, updates);
	}

	/** Set a ref, as a push would. */
	setRef(name: string, ref: string, commit: string) {
		const refs = this.refsOf.get(name) ?? new Map<string, string>();
		this.refsOf.set(name, refs);
		refs.set(ref, commit);
	}

	/** Commit a file to a repo's trunk, as a merge would. */
	write(name: string, path: string, text: string) {
		this.repos.get(name)!.set(path, text);
	}

	url(name: string) {
		return `https://acct.artifacts.example/git/newgit/${name}.git`;
	}

	/** The repo a URL from `url` names. */
	nameOf(url: string) {
		return url.replace("https://acct.artifacts.example/git/newgit/", "").replace(/\.git$/, "");
	}
}

/**
 * The endpoint's copy into a fork, done at once: what the container does in
 * the background, for tests that fork but are not about the copy. It starts
 * the copy (mints its passwords) and copies the source's files over.
 */
export function fakeCopy(repos: FakeRepos) {
	return async (_project: string, plan: CopyPlan, credentials: () => Promise<Pick<CopyPlan, "from" | "to">>) => {
		await credentials();
		const from = repos.repos.get(repos.nameOf(plan.from.url));
		const to = repos.repos.get(repos.nameOf(plan.to.url));
		if (!from || !to) throw new Error(`fake copy: ${plan.from.url} or ${plan.to.url} is missing`);
		for (const [path, text] of from) to.set(path, text);
		return { branches: { main: "0".repeat(40) } } satisfies CopyAnswer;
	};
}

/** For tests that never reach the Git endpoint. */
export const noEndpoint = async () => new Response("no endpoint in this test", { status: 501 });

const TYPES: Record<string, TreeEntry["type"]> = {
	"40000": "tree",
	"100644": "blob",
	"100755": "exec",
	"120000": "symlink",
	"160000": "gitlink",
};

/** A tree object's entries, as Artifacts' `readTree` would list them. */
function parseTree(body: Uint8Array): TreeEntry[] {
	const entries: TreeEntry[] = [];
	let at = 0;
	while (at < body.length) {
		const nul = body.indexOf(0, at);
		const [mode, name] = new TextDecoder().decode(body.subarray(at, nul)).split(/ (.*)/s);
		const hash = [...body.subarray(nul + 1, nul + 21)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
		entries.push({ name, hash, type: TYPES[mode] });
		at = nul + 21;
	}
	return entries;
}

/** A commit object's tree, parents, and committer time. */
function parseCommit(hash: string, body: Uint8Array): CommitFacts {
	const head = new TextDecoder().decode(body).split("\n\n")[0].split("\n");
	const field = (name: string) => head.filter((line) => line.startsWith(`${name} `)).map((line) => line.slice(name.length + 1));
	const committer = field("committer")[0] ?? "";
	return {
		hash,
		tree: field("tree")[0],
		parents: field("parent"),
		committedAt: Number(committer.split(" ").at(-2)),
	};
}
