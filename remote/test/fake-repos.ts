import type { MintedToken, Repos } from "../src/artifacts";

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
	async fork(from: string, to: string) {
		const source = this.repos.get(from);
		if (!source) throw Object.assign(new Error("missing"), { code: "NOT_FOUND" });
		this.repos.set(to, new Map(source));
		return { remote: this.url(to) };
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
}

/** For tests that never reach the Git endpoint. */
export const noEndpoint = async () => new Response("no endpoint in this test", { status: 501 });
