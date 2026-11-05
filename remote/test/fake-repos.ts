import type { MintedToken, Repos } from "../src/artifacts";
import type { CopyAnswer, CopyPlan } from "../src/endpoint";

/** The password the fake mints for a repo and scope: recognisable, not random. */
export const fakePassword = (name: string, scope: string) => ["pw", name, scope].join("-");

/** Artifacts in memory: repos are file maps; tokens are recognisable strings. */
export class FakeRepos implements Repos {
	readonly repos = new Map<string, Map<string, string>>();
	readonly minted: { name: string; scope: string; id: string }[] = [];
	readonly revokedTokens = new Set<string>();

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
		const id = `art-${this.minted.length + 1}`;
		this.minted.push({ name, scope, id });
		return { id, password: fakePassword(name, scope), expiresAt: "2099-01-01T00:00:00Z" };
	}
	async revokeToken(_name: string, tokenId: string) {
		this.revokedTokens.add(tokenId);
		return true;
	}
	async readFile(name: string, _ref: string, path: string) {
		return this.repos.get(name)?.get(path) ?? null;
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
