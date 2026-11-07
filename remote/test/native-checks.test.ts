import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { BackgroundChecks } from "../src/endpoint";
import { handle, issue } from "../src/index";
import { landingRef } from "../src/refs";
import { ASK_GIT, WALK_LIMIT, isAncestor, nativeAncestry, nativeIndex } from "../src/walk";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A merge's checks answered in the Worker from Artifacts' reads, not by the
// container's Git while the client polls (src/walk.ts). New tests live in
// their own file per AGENTS.md's "Working in parallel".

const sha = (c: string) => c.repeat(40);
const repoOf = (tracker: string) => `repo-${tracker}`;

let repos: FakeRepos;
let trees = 0;

/** A tracker commit whose tree pins `pins` at `.newgit/pins/`. */
function commit(tracker: string, hash: string, pins: Record<string, string>, parents: string[] = [], at = 1_000_000) {
	const tree = (0x100000 + ++trees).toString(16).padStart(40, "0");
	const newgit = (0x100000 + ++trees).toString(16).padStart(40, "0");
	const pinsTree = (0x100000 + ++trees).toString(16).padStart(40, "0");
	repos.addTree(repoOf(tracker), tree, [
		{ name: "src", hash: sha("9"), type: "tree" },
		{ name: ".newgit", hash: newgit, type: "tree" },
	]);
	repos.addTree(repoOf(tracker), newgit, [{ name: "pins", hash: pinsTree, type: "tree" }]);
	repos.addTree(
		repoOf(tracker),
		pinsTree,
		Object.entries(pins).map(([name, pin]) => ({ name, hash: pin, type: "gitlink" as const })),
	);
	repos.addCommit(repoOf(tracker), { hash, tree, parents, committedAt: at });
}

beforeEach(() => {
	repos = new FakeRepos();
	trees = 0;
});

describe("an index read in the Worker", () => {
	it("is every pin, all the way down, with nested trackers by their IDs", async () => {
		commit("root", sha("a"), { t3code: sha("b"), access: sha("c") });
		commit("t3code", sha("b"), { repos: sha("d") });
		commit("access", sha("c"), {});
		commit("t3code__repos", sha("d"), {});
		const list = { root: sha("a"), t3code: sha("b"), access: sha("c"), t3code__repos: sha("d") };
		expect(await nativeIndex(repos, repoOf, list)).toEqual({ matches: true, actual: list });
		const wrong = await nativeIndex(repos, repoOf, { ...list, t3code: sha("f") });
		expect(wrong).toMatchObject({ matches: false, actual: list });
	});

	it("does not match a list that names a tracker root's commit does not pin", async () => {
		commit("root", sha("a"), {});
		expect(await nativeIndex(repos, repoOf, { root: sha("a"), extra: sha("b") })).toMatchObject({ matches: false });
	});

	it("asks Git when a commit cannot be read", async () => {
		commit("root", sha("a"), { t3code: sha("b") });
		expect(await nativeIndex(repos, repoOf, { root: sha("a"), t3code: sha("b") })).toBe(ASK_GIT);
	});
});

describe("ancestry read in the Worker", () => {
	/** `count` commits in a line, oldest first, an hour apart. */
	function line(tracker: string, count: number, start = 0): string[] {
		const hashes = Array.from({ length: count }, (_, index) => (0x200000 + start + index + 1).toString(16).padStart(40, "0"));
		hashes.forEach((hash, index) => commit(tracker, hash, {}, index === 0 ? [] : [hashes[index - 1]], 1_000_000 + index * 3600));
		return hashes;
	}

	it("walks every parent: a merge's second parent counts", async () => {
		const main = line("t3code", 3);
		const side = line("t3code", 2, 100);
		commit("t3code", sha("f"), {}, [main[2], side[1]], 2_000_000);
		expect(await isAncestor(repos, repoOf("t3code"), side[0], sha("f"))).toBe(true);
		expect(await isAncestor(repos, repoOf("t3code"), main[0], sha("f"))).toBe(true);
		expect(await isAncestor(repos, repoOf("t3code"), sha("f"), main[2]), "not backwards").toBe(false);
		expect(await isAncestor(repos, repoOf("t3code"), side[1], main[2]), "not sideways").toBe(false);
	});

	it("answers forward and on `main` as the container does", async () => {
		const hashes = line("t3code", 4);
		repos.setRef(repoOf("t3code"), "refs/heads/main", hashes[2]);
		const answers = await nativeAncestry(repos, repoOf, [
			{ tracker: "t3code", old: hashes[0], new: hashes[2] },
		]);
		expect(answers).toEqual([{ tracker: "t3code", forward: true, onMain: true }]);
		const ahead = await nativeAncestry(repos, repoOf, [{ tracker: "t3code", old: hashes[0], new: hashes[3] }]);
		expect(ahead, "past `main`").toEqual([{ tracker: "t3code", forward: true, onMain: false }]);
	});

	it("stops below the target's date, so a sideways move does not walk all history", async () => {
		const old = line("t3code", 150);
		const side = (0x300000).toString(16).padStart(40, "0");
		commit("t3code", side, {}, [old[0]], 1_000_000 + 160 * 3600);
		repos.reads = 0;
		expect(await isAncestor(repos, repoOf("t3code"), old[149], side)).toBe(false);
		expect(repos.reads).toBeLessThan(10);
	});

	it("asks Git past its walk limit, or for a commit it cannot read", async () => {
		const long = line("t3code", WALK_LIMIT + 20);
		expect(await isAncestor(repos, repoOf("t3code"), long[0], long.at(-1)!)).toBe(ASK_GIT);
		expect(await isAncestor(repos, repoOf("t3code"), sha("0"), long[1])).toBe(ASK_GIT);
	});
});

describe("a merge whose objects the Worker can read", () => {
	let jack: string;
	let name: string;
	let containerAsked: string[];

	const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;
	async function call(method: string, route: string, body?: unknown) {
		const response = await handle(
			new Request(`https://newgit.dev${route}`, {
				method,
				headers: { "content-type": "application/json", authorization: `Bearer ${jack}` },
				body: body === undefined ? undefined : JSON.stringify(body),
			}),
			env,
			{
				repos,
				fetcher: fetch,
				waitUntil: () => {},
				endpoint: noEndpoint,
				ancestry: async () => {
					containerAsked.push("ancestry");
					throw new Error("the container was asked");
				},
				index: async () => {
					containerAsked.push("index");
					throw new Error("the container was asked");
				},
			},
		);
		return { status: response.status, body: (await response.json()) as any };
	}

	it("lands without asking the container", async () => {
		containerAsked = [];
		name = `native-${crypto.randomUUID().slice(0, 8)}`;
		jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		await call("POST", "/v1/projects", { name, trackers: ["root", "access", "t3code"] });
		const defs = `jack.${name}.t.access`;
		repos.write(defs, "root.toml", 'audience = "project-devs"\ngit = true\n');
		repos.write(defs, "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\n');
		const repo = (tracker: string) => `jack.${name}.t.${tracker}`;
		const at = (tracker: string) => (hash: string, pins: Record<string, string>, parents: string[] = []) =>
			commitIn(repo(tracker), hash, pins, parents);
		const main = { root: sha("a"), t3code: sha("b") };
		await call("PUT", path("/branches/main"), { list: main });
		at("t3code")(sha("b"), {});
		at("t3code")(sha("d"), {}, [sha("b")]);
		repos.setRef(repo("t3code"), "refs/heads/main", sha("d"));
		at("root")(sha("c"), { t3code: sha("d") }, [sha("a")]);
		const moved = { root: sha("c"), t3code: sha("d") };
		expect((await call("POST", path("/branches/main/merge"), { base: main, list: moved })).status).toBe(200);
		repos.setRef(repo("root"), landingRef(sha("c")), sha("c"));
		const landed = await call("PUT", path("/branches/main"), { base: main, list: moved });
		expect(landed.status, JSON.stringify(landed.body)).toBe(200);
		expect(landed.body.list).toEqual(moved);
		expect(containerAsked).toEqual([]);
	});
});

/** `commit`, in a repo named outright. */
function commitIn(repo: string, hash: string, pins: Record<string, string>, parents: string[] = []) {
	const tree = (0x100000 + ++trees).toString(16).padStart(40, "0");
	const newgit = (0x100000 + ++trees).toString(16).padStart(40, "0");
	const pinsTree = (0x100000 + ++trees).toString(16).padStart(40, "0");
	repos.addTree(repo, tree, [{ name: ".newgit", hash: newgit, type: "tree" }]);
	repos.addTree(repo, newgit, [{ name: "pins", hash: pinsTree, type: "tree" }]);
	repos.addTree(
		repo,
		pinsTree,
		Object.entries(pins).map(([name, pin]) => ({ name, hash: pin, type: "gitlink" as const })),
	);
	repos.addCommit(repo, { hash, tree, parents, committedAt: 1_000_000 });
}

describe("a background check's request", () => {
	it("waits for a short check and stops waiting at its limit", async () => {
		const checks = new BackgroundChecks();
		let finish = () => {};
		void checks.start("k", () => new Promise((resolve) => (finish = () => resolve({ ok: true, json: "{}" }))));
		const started = Date.now();
		setTimeout(() => finish(), 50);
		await checks.settled("k", 5_000);
		expect(Date.now() - started).toBeLessThan(2_000);
		expect(checks.collect("k")).toEqual({ ok: true, json: "{}" });

		void checks.start("slow", () => new Promise(() => {}));
		const waited = Date.now();
		await checks.settled("slow", 100);
		expect(Date.now() - waited).toBeLessThan(2_000);
		expect(checks.collect("slow")).toBe("running");
	});
});
