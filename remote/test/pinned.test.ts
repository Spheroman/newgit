import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { commitObject, treeObject } from "../src/objects";
import { landingRef } from "../src/refs";
import { nativeIndex } from "../src/walk";
import { FakeRepos, noEndpoint } from "./fake-repos";
import { sweep } from "../src/sweep";

// A pinned tracker (`pinned = true`, docs/design/anchoring.md, *Follow and
// pinned*): never rolled up into its parent, and moving its pin is a change
// to the parent, the parent's maintainers' to approve. New tests live in
// their own file per AGENTS.md's "Working in parallel".

const blob = "45b983be36b73c0788dc9cbcb76cbb80fc7bb057";
const other = "8baef1b4abc478178b004d62031cf7fe6db6f903";

let repos: FakeRepos;
let slug: string;
let jack: string;
let ana: string;
const pending: Promise<unknown>[] = [];
const repo = (tracker: string) => `jack.${slug}.t.${tracker}`;
const path = (rest: string) => `/v1/projects/jack/${slug}${rest}`;

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: (promise) => void pending.push(promise), endpoint: noEndpoint },
	);
	const answer = { status: response.status, body: (await response.json()) as any };
	await Promise.all(pending.splice(0));
	return answer;
}

let clock = 1_790_000_000;
/** A tracker's commit: `file` at its root, and `pins` at `.newgit/pins/`. */
async function commitIn(tracker: string, pins: Record<string, string>, parents: string[], file = blob) {
	const pinsTree = await treeObject(Object.entries(pins).map(([name, hash]) => ({ name, hash, type: "gitlink" as const })));
	const newgit = await treeObject([{ name: "pins", hash: pinsTree.hash, type: "tree" }]);
	const tree = await treeObject([
		{ name: "file.txt", hash: file, type: "blob" },
		{ name: ".newgit", hash: newgit.hash, type: "tree" },
	]);
	const at = clock++;
	const commit = await commitObject({
		tree: tree.hash,
		parents,
		author: { name: "jack", email: "jack@example.com", at },
		committer: { name: "jack", email: "jack@example.com", at },
		message: `work in ${tracker}`,
	});
	await repos.updateRefs(repo(tracker), [], [pinsTree, newgit, tree, commit]);
	return commit.hash;
}

let main: Record<string, string>;

beforeEach(async () => {
	repos = new FakeRepos();
	slug = `anchor-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "t3code", "site"] });
	await call(jack, "POST", path("/members"), { login: "ana" });
	// Root is the owner's to maintain; the children are anyone's in the project.
	repos.write(repo("access"), "root.toml", 'audience = "project-devs"\ngit = true\nmaintainers = "owner"\n');
	repos.write(repo("access"), "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\n');
	repos.write(repo("access"), "site.toml", 'audience = "project-devs"\ngit = true\nmount = "site"\n');
	const t3code = await commitIn("t3code", {}, []);
	const site = await commitIn("site", {}, []);
	const access = await commitIn("access", {}, []);
	const root = await commitIn("root", { t3code, site, access }, []);
	main = { root, t3code, site, access };
	expect((await call(jack, "PUT", path("/branches/main"), { list: main })).status).toBe(201);
	for (const [tracker, commit] of Object.entries(main)) repos.setRef(repo(tracker), "refs/heads/main", commit);
});

const project = () => env.PROJECTS.getByName(`jack/${slug}`);
const deps = () => ({ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint });
const pin = () => repos.write(repo("access"), "site.toml", 'audience = "project-devs"\ngit = true\nmount = "site"\npinned = true\n');

describe("a pinned tracker", () => {
	it("moves only as a change to its parent: promoting it is root's maintainers' to approve", async () => {
		pin();
		const next = await commitIn("site", {}, [main.site], other);
		repos.setRef(repo("site"), "refs/heads/main", next);
		const root = await commitIn("root", { t3code: main.t3code, site: next, access: main.access }, [main.root]);
		const list = { ...main, root, site: next };
		expect((await call(ana, "POST", path("/branches/main/merge"), { base: main, list })).status, "ana maintains site, not root").toBe(403);
		const begun = await call(jack, "POST", path("/branches/main/merge"), { base: main, list });
		expect(begun.status).toBe(200);
		expect(begun.body.anchor).toBe("root");
	});

	it("is never rolled up into its parent, and nothing is left pending for it", async () => {
		pin();
		const before = (await project().branch("main"))!.list.root;
		const next = await commitIn("site", {}, [main.site], other);
		expect(await project().moveHead("main", "site", main.site, next, "jack")).toBe(true);
		const t3 = await commitIn("t3code", {}, [main.t3code], other);
		expect(await project().moveHead("main", "t3code", main.t3code, t3, "jack")).toBe(true);
		await sweep(env, deps(), { owner: "jack", name: slug });
		expect(await project().hasPending("main")).toBe(false);
		const now = (await project().branch("main"))!.list;
		expect(now.root, "t3code, beside it, rolled up").not.toBe(before);
		const pins = (await nativeIndex(repos, (tracker) => repo(tracker), { root: now.root })) as {
			actual: Record<string, string>;
		};
		expect(pins.actual.site, "site held where root pins it").toBe(main.site);
		expect(pins.actual.t3code).toBe(t3);
	});

	it("held behind its own `main` is said", async () => {
		pin();
		const ahead = await commitIn("site", {}, [main.site], other);
		repos.setRef(repo("site"), "refs/heads/main", ahead);
		const state = (await call(jack, "GET", path("/state"))).body;
		const branch = state.branches.find((candidate: { name: string }) => candidate.name === "main");
		expect(branch.pinned).toEqual([{ tracker: "site", pin: main.site, main: ahead }]);
	});

	it("not pinned, says nothing of its `main`", async () => {
		const ahead = await commitIn("site", {}, [main.site], other);
		repos.setRef(repo("site"), "refs/heads/main", ahead);
		const state = (await call(jack, "GET", path("/state"))).body;
		expect(state.branches.find((candidate: { name: string }) => candidate.name === "main").pinned).toBeUndefined();
	});
});
