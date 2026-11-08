import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { commitObject, treeObject } from "../src/objects";
import { landingRef } from "../src/refs";
import { nativeIndex } from "../src/walk";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A merge lands at its anchor, the lowest tracker covering everything it
// touches, and root rolls up afterwards (docs/design/anchoring.md, build
// order step 4). Two merges at different anchors no longer queue on root.
// New tests live in their own file per AGENTS.md's "Working in parallel".

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

/**
 * What a client's merge of a change to `tracker` sends: the tracker's new
 * commit, on its `main` already (as a catch-up leaves it), and root's commit
 * pinning it, at its landing ref.
 */
async function change(tracker: string, base: Record<string, string>, token = jack, file = other, onMain = true) {
	const next = await commitIn(tracker, {}, [base[tracker]], file);
	if (onMain) repos.setRef(repo(tracker), "refs/heads/main", next);
	const pins = Object.fromEntries(Object.entries(base).filter(([name]) => name !== "root"));
	const root = await commitIn("root", { ...pins, [tracker]: next }, [base.root]);
	const list = { ...base, root, [tracker]: next };
	const begun = await call(token, "POST", path("/branches/main/merge"), { base, list });
	repos.setRef(repo("root"), landingRef(root), root);
	return { begun, list, finish: () => call(token, "PUT", path("/branches/main"), { base, list }) };
}

const heads = async () => (await call(jack, "GET", path("/branches/main"))).body;

describe("a merge that touches one tracker", () => {
	it("lands at that tracker, and root rolls up to the merge's own commit", async () => {
		const merge = await change("t3code", main);
		expect(merge.begun.status, JSON.stringify(merge.begun.body)).toBe(200);
		expect(merge.begun.body.anchor).toBe("t3code");
		expect((await merge.finish()).status).toBe(200);
		const now = await heads();
		expect(now.list.t3code).toBe(merge.list.t3code);
		// Root's head is the merge's own root commit, fast-forwarded to.
		expect(now.list.root).toBe(merge.list.root);
		expect(now.behind).toBeUndefined();
		expect((await repos.refs(repo("root")))["refs/heads/main"], "root's branch followed").toBe(merge.list.root);
		expect(await nativeIndex(repos, (tracker) => repo(tracker), now.list)).toMatchObject({ matches: true });
	});

	it("needs no say from root's maintainers: only the touched tracker's", async () => {
		// New to `t3code`, not on its `main`: `t3code`'s maintainers' to land.
		const merge = await change("t3code", main, ana, other, false);
		expect(merge.begun.status, JSON.stringify(merge.begun.body)).toBe(200);
		expect(merge.begun.body.credentials.map((credential: { tracker: string }) => credential.tracker).sort()).toEqual([
			"root",
			"t3code",
		]);
		expect((await merge.finish()).status).toBe(200);
	});

	it("leaves a pin following its tracker's `main` to the parent's maintainers", async () => {
		const merge = await change("t3code", main, ana);
		expect(merge.begun.status).toBe(403);
		expect(merge.begun.body.error).toContain("`root`'s to choose");
	});
});

describe("a merge two levels down", () => {
	it("lands at the grandchild, and its commits above are sent and rolled up", async () => {
		repos.write(repo("t3code"), ".newgit/trackers/repos.toml", 'audience = "project-devs"\ngit = true\nmount = ".repos"\n');
		await call(jack, "POST", path("/trackers"), { name: "t3code__repos" });
		const repos0 = await commitIn("t3code__repos", {}, []);
		const t3code = await commitIn("t3code", { repos: repos0 }, [main.t3code]);
		const root = await commitIn("root", { t3code, site: main.site, access: main.access }, [main.root]);
		const start = { ...main, root, t3code, t3code__repos: repos0 };
		// Grown by a merge at root: a tracker joins.
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, list: start })).status).toBe(200);
		repos.setRef(repo("root"), landingRef(root), root);
		const grown = await call(jack, "PUT", path("/branches/main"), { base: main, list: start });
		expect(grown.status, JSON.stringify(grown.body)).toBe(200);

		const repos1 = await commitIn("t3code__repos", {}, [repos0], other);
		repos.setRef(repo("t3code__repos"), "refs/heads/main", repos1);
		const t3code1 = await commitIn("t3code", { repos: repos1 }, [t3code]);
		const root1 = await commitIn("root", { t3code: t3code1, site: main.site, access: main.access }, [root]);
		const list = { ...start, root: root1, t3code: t3code1, t3code__repos: repos1 };
		const begun = await call(jack, "POST", path("/branches/main/merge"), { base: start, list });
		expect(begun.status, JSON.stringify(begun.body)).toBe(200);
		expect(begun.body.anchor).toBe("t3code__repos");
		const handed = begun.body.credentials.map((credential: { tracker: string }) => credential.tracker).sort();
		expect(handed, "the commit between is sent too").toEqual(["root", "t3code"]);
		repos.setRef(repo("root"), landingRef(root1), root1);
		expect((await call(jack, "PUT", path("/branches/main"), { base: start, list })).status).toBe(200);
		const now = await heads();
		expect(now.list.t3code__repos).toBe(repos1);
		expect(await nativeIndex(repos, (tracker) => repo(tracker), now.list)).toMatchObject({ matches: true });
	});
});

describe("a change's own commit in root", () => {
	it("joins root's history only as exactly a pin move: a smuggled parent is left out", async () => {
		const next = await commitIn("t3code", {}, [main.t3code], other);
		repos.setRef(repo("t3code"), "refs/heads/main", next);
		const smuggled = await commitIn("root", {}, [], other);
		// The right tree, but a second parent no one approved.
		const pins = { t3code: next, site: main.site, access: main.access };
		const crafted = await commitIn("root", pins, [main.root, smuggled]);
		const list = { ...main, root: crafted, t3code: next };
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, list })).status).toBe(200);
		repos.setRef(repo("root"), landingRef(crafted), crafted);
		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list })).status).toBe(200);
		const head = (await heads()).list.root;
		expect(head, "not fast-forwarded to").not.toBe(crafted);
		const made = await repos.readCommit(repo("root"), head);
		expect(made?.parents).toEqual([main.root]);
	});
});

describe("two merges at different anchors", () => {
	it("both land: neither waits on root", async () => {
		const one = await change("t3code", main);
		const two = await change("site", main);
		expect(one.begun.body.anchor).toBe("t3code");
		expect(two.begun.body.anchor).toBe("site");
		expect((await one.finish()).status).toBe(200);
		const second = await two.finish();
		expect(second.status, JSON.stringify(second.body)).toBe(200);
		const now = await heads();
		expect(now.list.t3code).toBe(one.list.t3code);
		expect(now.list.site).toBe(two.list.site);
		expect(await nativeIndex(repos, (tracker) => repo(tracker), now.list)).toMatchObject({ matches: true });
		// The second merge's own root commit is kept, merged with the first.
		const head = await repos.readCommit(repo("root"), now.list.root);
		expect(head?.parents).toEqual([one.list.root, two.list.root]);
	});

	it("at the same anchor, still meet: the second finds it moved", async () => {
		const one = await change("t3code", main);
		const two = await change("t3code", main, jack, blob.replace("4", "5"));
		expect((await one.finish()).status).toBe(200);
		const second = await two.finish();
		expect(second.status).toBe(409);
	});
});

describe("a merge that touches root", () => {
	it("lands at root, and asks root's maintainers", async () => {
		const root = await commitIn("root", { t3code: main.t3code, site: main.site, access: main.access }, [main.root], other);
		const list = { ...main, root };
		const theirs = await call(ana, "POST", path("/branches/main/merge"), { base: main, list });
		expect(theirs.status).toBe(403);
		const ours = await call(jack, "POST", path("/branches/main/merge"), { base: main, list });
		expect(ours.status).toBe(200);
		expect(ours.body.anchor).toBe("root");
	});
});

describe("a read while root cannot roll up", () => {
	it("serves root's last commit, and says it is behind", async () => {
		const updateRefs = repos.updateRefs.bind(repos);
		const quiet = console.error;
		console.error = () => {};
		try {
			// Root has moved on meanwhile (another change rolled up), so this
			// merge's own commit cannot be fast-forwarded to: the roll-up must
			// write one, and cannot.
			const first = await change("site", main);
			expect((await first.finish()).status).toBe(200);
			repos.updateRefs = async (name, updates, objects) => {
				if (name === repo("root") && updates.length > 0 && objects !== undefined && objects.length > 0)
					throw new Error("Artifacts is down");
				return updateRefs(name, updates, objects);
			};
			const base = (await heads()).list;
			const merge = await change("t3code", main);
			expect(merge.begun.body.anchor).toBe("t3code");
			expect((await merge.finish()).status).toBe(200);
			const now = await heads();
			expect(now.behind).toBe(true);
			expect(now.list.t3code, "as root's last commit pins it").toBe(base.t3code);
			expect(now.list.root).toBe(base.root);
		} finally {
			console.error = quiet;
		}
	});
});
