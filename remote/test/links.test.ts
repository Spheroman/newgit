import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { commitObject, treeObject } from "../src/objects";
import { landingRef } from "../src/refs";
import { nativeIndex } from "../src/walk";
import { FakeRepos, noEndpoint } from "./fake-repos";

// One tracker in two roots (docs/design/trackers.md, *Links*): a tracker
// defined with `link = "<owner>/<project>"` is that project's tracker, its
// history, heads and repo there; each root linking it pins its own commit of
// it, and a move made from any of them reaches all of them. New tests live
// in their own file per AGENTS.md's "Working in parallel".

const blob = "45b983be36b73c0788dc9cbcb76cbb80fc7bb057";
const other = "8baef1b4abc478178b004d62031cf7fe6db6f903";

let repos: FakeRepos;
let jack: string;
let ana: string;
let home: string;
let roots: [string, string];
let mains: Record<string, Record<string, string>>;
const pending: Promise<unknown>[] = [];
const repo = (project: string, tracker: string) => `jack.${project}.t.${tracker}`;
const path = (project: string, rest: string) => `/v1/projects/jack/${project}${rest}`;

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
/** A commit in `name`: `file` at its root, and `pins` at `.newgit/pins/`. */
async function commitIn(name: string, pins: Record<string, string>, parents: string[], file = blob) {
	const pinsTree = await treeObject(Object.entries(pins).map(([pin, hash]) => ({ name: pin, hash, type: "gitlink" as const })));
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
		message: `work in ${name}`,
	});
	await repos.updateRefs(name, [], [pinsTree, newgit, tree, commit]);
	return commit.hash;
}

/** A project with root, access and `env` (defined by `access`), its `main` landed. */
async function project(name: string, access = "", own = false): Promise<Record<string, string>> {
	await call(jack, "POST", "/v1/projects", { name, trackers: own ? ["root", "access", "env"] : ["root", "access"] });
	repos.write(repo(name, "access"), "root.toml", 'audience = "project-devs"\ngit = true\nmaintainers = "owner"\n');
	if (access !== "") repos.write(repo(name, "access"), "env.toml", access);
	const accessCommit = await commitIn(repo(name, "access"), {}, []);
	const children: Record<string, string> = { access: accessCommit };
	if (own) children.env = await commitIn(repo(name, "env"), {}, []);
	const root = await commitIn(repo(name, "root"), children, []);
	const list = { root, ...children };
	expect((await call(jack, "PUT", path(name, "/branches/main"), { list })).status).toBe(201);
	for (const [tracker, commit] of Object.entries(list)) repos.setRef(repo(name, tracker), "refs/heads/main", commit);
	return list;
}

/** Root `name` takes in the link at the home's `main`, by a merge at root. */
async function link(name: string): Promise<Record<string, string>> {
	const recorded = await call(jack, "POST", path(name, "/trackers"), { name: "env", link: `jack/${home}/env` });
	expect(recorded.status, JSON.stringify(recorded.body)).toBe(201);
	const base = mains[name];
	const at = mains[home].env;
	// Its definition is new to `access` in the same merge.
	const access = await commitIn(repo(name, "access"), {}, [base.access], other);
	const root = await commitIn(repo(name, "root"), { access, env: at }, [base.root]);
	const list = { ...base, root, access, env: at };
	const begun = await call(jack, "POST", path(name, "/branches/main/merge"), { base, list });
	expect(begun.status, JSON.stringify(begun.body)).toBe(200);
	repos.setRef(repo(name, "root"), landingRef(root), root);
	repos.setRef(repo(name, "access"), landingRef(access), access);
	// Landed, `access`'s `main` is where the definition is.
	repos.write(repo(name, "access"), "env.toml", linked.replace("HOME", home));
	repos.setRef(repo(name, "access"), "refs/heads/main", access);
	const landed = await call(jack, "PUT", path(name, "/branches/main"), { base, list });
	expect(landed.status, JSON.stringify(landed.body)).toBe(200);
	return list;
}

const linked = 'audience = "project-devs"\ngit = true\nmount = ".env"\nlink = "jack/HOME/env"\n';
const branch = async (name: string) => (await call(jack, "GET", path(name, "/branches/main"))).body;
const namer = (name: string) => (tracker: string) => (tracker === "env" ? repo(home, "env") : repo(name, tracker));

beforeEach(async () => {
	repos = new FakeRepos();
	const id = crypto.randomUUID().slice(0, 8);
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	home = `env-${id}`;
	roots = [`one-${id}`, `two-${id}`];
	mains = { [home]: await project(home, 'audience = "project-devs"\ngit = true\nmount = "env"\n', true) };
	// The link's definition is not on `access`'s `main` until its merge lands.
	for (const name of roots) mains[name] = await project(name);
	for (const name of roots) mains[name] = await link(name);
});

describe("a tracker linked into two roots", () => {
	it("is its home's: one head, read through each root", async () => {
		for (const name of roots) {
			const now = await branch(name);
			expect(now.list.env).toBe(mains[home].env);
			expect(await nativeIndex(repos, namer(name), now.list)).toMatchObject({ matches: true });
		}
	});

	it("moved from one root, rolls up in both", async () => {
		const [one, two] = roots;
		const base = mains[one];
		// The change, new to the home's history: its home's maintainers' to land.
		const next = await commitIn(repo(home, "env"), {}, [base.env], other);
		const root = await commitIn(repo(one, "root"), { access: base.access, env: next }, [base.root]);
		const list = { ...base, root, env: next };
		const begun = await call(jack, "POST", path(one, "/branches/main/merge"), { base, list });
		expect(begun.status, JSON.stringify(begun.body)).toBe(200);
		expect(begun.body.anchor).toBe("env");
		repos.setRef(repo(home, "env"), landingRef(next), next);
		repos.setRef(repo(one, "root"), landingRef(root), root);
		const landed = await call(jack, "PUT", path(one, "/branches/main"), { base, list });
		expect(landed.status, JSON.stringify(landed.body)).toBe(200);

		// The home's repo follows, though this merge asked no branch to move:
		// the next root to read it reads where it is.
		expect((await repos.refs(repo(home, "env")))["refs/heads/main"]).toBe(next);
		const there = await branch(home);
		expect(there.list.env, "the home's own head moved").toBe(next);
		expect(there.list.root, "and the home's root rolled it up too").not.toBe(mains[home].root);
		expect(await nativeIndex(repos, (tracker) => repo(home, tracker), there.list)).toMatchObject({ matches: true });
		for (const name of [one, two]) {
			const now = await branch(name);
			expect(now.list.env, name).toBe(next);
			expect(now.list.root, `${name}'s root rolled up`).not.toBe(mains[name].root);
			expect(await nativeIndex(repos, namer(name), now.list)).toMatchObject({ matches: true });
		}
	});

	it("joins a root only at its home's head", async () => {
		const name = `three-${crypto.randomUUID().slice(0, 8)}`;
		const base = await project(name, linked.replace("HOME", home));
		expect((await call(jack, "POST", path(name, "/trackers"), { name: "env", link: `jack/${home}/env` })).status).toBe(201);
		const stale = await commitIn(repo(home, "env"), {}, [mains[home].env], other);
		const root = await commitIn(repo(name, "root"), { access: base.access, env: stale }, [base.root]);
		const list = { ...base, root, env: stale };
		await call(jack, "POST", path(name, "/branches/main/merge"), { base, list });
		repos.setRef(repo(name, "root"), landingRef(root), root);
		expect((await call(jack, "PUT", path(name, "/branches/main"), { base, list })).status).toBe(409);
		expect((await branch(home)).list.env, "and its home never moved").toBe(mains[home].env);
	});

	it("is linked only by a reader of its home", async () => {
		const name = `ana-${crypto.randomUUID().slice(0, 8)}`;
		await call(ana, "POST", "/v1/projects", { name, trackers: ["root", "access"] });
		const refused = await call(ana, "POST", `/v1/projects/ana/${name}/trackers`, { name: "env", link: `jack/${home}/env` });
		expect(refused.status).toBe(403);
		expect((await call(jack, "POST", path(roots[0], "/trackers"), { name: "self", link: `jack/${roots[0]}` })).status).toBe(400);
	});

	it("outlives a root that linked it", async () => {
		expect((await call(jack, "DELETE", path(roots[0], ""))).status).toBeLessThan(300);
		expect((await branch(home)).list.env).toBe(mains[home].env);
		expect((await branch(roots[1])).list.env).toBe(mains[home].env);
	});

	it("hands out its home's repo to a root linking it, and no other", async () => {
		const asked = await call(jack, "POST", path(roots[1], "/credentials"), {
			repos: [repo(home, "env")],
			scope: "read",
		});
		expect(asked.body.credentials[0].status, JSON.stringify(asked.body)).toBeUndefined();
		expect(asked.body.credentials[0].password).toBeDefined();
		const stranger = `four-${crypto.randomUUID().slice(0, 8)}`;
		await project(stranger);
		const refused = await call(jack, "POST", path(stranger, "/credentials"), {
			repos: [repo(home, "env")],
			scope: "read",
		});
		expect(refused.body.credentials[0].status).toBe(400);
	});
});
