import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

let repos: FakeRepos;
let name: string;
let jack: string;
let ana: string;

const sha = (c: string) => c.repeat(40);
const main = { root: sha("a"), t3code: sha("b") };

beforeEach(async () => {
	repos = new FakeRepos();
	moves = {};
	asked = [];
	pinned = null;
	name = `web-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "t3code"] });
	const defs = `jack.${name}.t.access`;
	repos.write(defs, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(defs, "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\nmaintainers = "user:jack"\n');
	repos.write(defs, "env.toml", 'audience = "project-devs"\n');
	await call(jack, "PUT", path("/branches/main"), { list: main });
	await call(jack, "POST", path("/members"), { login: "ana" });
});

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;

/**
 * What the fake container answers for each pin a merge moves: forward along
 * the tracker's `main` unless named otherwise. Every call is recorded.
 */
let moves: Record<string, { forward: boolean; onMain: boolean }> = {};
let asked: { tracker: string; old: string; new: string }[][] = [];
const fakeAncestry = async (_project: string, plan: { checks: { tracker: string; old: string; new: string }[] }) => {
	asked.push(plan.checks);
	return plan.checks.map((check) => ({ tracker: check.tracker, ...(moves[check.tracker] ?? { forward: true, onMain: true }) }));
};

/** What the fake container says root's pushed commit pins; `null`: the index as sent. */
let pinned: Record<string, string> | null = null;
const fakeIndex = async (_project: string, plan: { list: Record<string, string> }) => {
	const actual = pinned ?? plan.list;
	return { matches: JSON.stringify(actual) === JSON.stringify(plan.list), actual };
};

const begin = (token: string, base: Record<string, string>, list: Record<string, string>, branch = "main") =>
	call(token, "POST", path(`/branches/${branch}/merge`), { base, list });

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint, ancestry: fakeAncestry, index: fakeIndex },
	);
	return { status: response.status, body: (await response.json()) as any };
}

describe("merging into a branch", () => {
	it("hands write passwords for root, then moves the index in one write", async () => {
		const moved = { ...main, root: sha("c") };
		const started = await begin(jack, main, moved);
		expect(started.status).toBe(200);
		expect(started.body.credentials.map((c: any) => c.repo)).toEqual([`jack.${name}.t.root`]);
		expect(repos.minted.at(-1)).toMatchObject({ name: `jack.${name}.t.root`, scope: "write" });
		const finished = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
		expect(finished.status).toBe(200);
		expect((await call(jack, "GET", path("/branches/main"))).body.list).toEqual(moved);
	});

	it("cancels its write passwords when it finishes, and lands only the index it began with", async () => {
		const moved = { ...main, root: sha("c") };
		const started = await begin(jack, main, moved);
		const minted = repos.minted.at(-1)!.id;
		const sneaky = await call(jack, "PUT", path("/branches/main"), {
			base: main,
			list: { root: sha("c"), t3code: sha("e") },
		});
		expect(sneaky.status).toBe(400);
		expect(sneaky.body.error).toContain("not the index a merge began with");
		expect(started.status).toBe(200);
		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list: moved })).status).toBe(200);
		expect(repos.revokedTokens.has(minted)).toBe(true);
	});

	it("is refused when root does not move", async () => {
		expect((await begin(jack, main, main)).status).toBe(400);
		expect((await begin(jack, main, { ...main, t3code: sha("c") })).status).toBe(400);
	});

	it("refuses a merge onto an index that has moved since", async () => {
		const stale = { ...main, root: sha("f") };
		const refused = await begin(jack, stale, { ...stale, root: sha("c") });
		expect(refused.status).toBe(409);
		expect(refused.body.error).toContain("merge again");
	});

	it("lets two merges begin at once, and lands whichever finishes first", async () => {
		const jacks = { ...main, root: sha("c") };
		const anas = { ...main, root: sha("d") };
		expect((await begin(jack, main, jacks)).status).toBe(200);
		expect((await begin(ana, main, anas)).status, "nothing is locked").toBe(200);
		// Ana cannot land the merge jack began.
		const sneak = await call(ana, "PUT", path("/branches/main"), { base: main, list: jacks });
		expect(sneak.status).toBe(409);
		expect(sneak.body.error).toContain("jack began this merge");
		expect((await call(ana, "PUT", path("/branches/main"), { base: main, list: anas })).status).toBe(200);
		const late = await call(jack, "PUT", path("/branches/main"), { base: main, list: jacks });
		expect(late.status).toBe(409);
		expect(late.body.error).toContain("merge again");
		expect((await call(jack, "GET", path("/branches/main"))).body.list).toEqual(anas);
	});

	it("is a person's decision: agent tokens are refused", async () => {
		const agent = (await call(jack, "POST", "/v1/tokens", { name: "bot" })).body.token;
		expect((await begin(agent, main, { ...main, root: sha("c") })).status).toBe(403);
	});

	it("asks Git how every child pin moves, and trusts nothing the client says about it", async () => {
		const moved = { root: sha("c"), t3code: sha("d") };
		// The client may still send what it thinks; the Worker does not read it.
		const response = await call(ana, "POST", path("/branches/main/merge"), {
			base: main,
			list: moved,
			trackers: ["root"],
			off_main: [],
		});
		moves = {};
		expect(response.status).toBe(200);
		expect(asked).toEqual([[{ tracker: "t3code", old: sha("b"), new: sha("d") }]]);
	});

	it("lets anyone who merges root move a child forward along its main, nothing else", async () => {
		// ana maintains root, not t3code (jack's alone).
		const moved = { root: sha("c"), t3code: sha("d") };
		const along = await begin(ana, main, moved);
		expect(along.status).toBe(200);
		expect(along.body.credentials.map((c: any) => c.tracker)).toEqual(["root"]);
		expect((await call(ana, "PUT", path("/branches/main"), { base: main, list: moved })).status).toBe(200);
		const after = (await call(ana, "GET", path("/branches/main"))).body.list;

		moves = { t3code: { forward: true, onMain: false } };
		const off = await begin(ana, after, { ...after, root: sha("e"), t3code: sha("f") });
		expect(off.status).toBe(403);
		expect(off.body.error).toContain("t3code");
		expect(off.body.error).toContain("not on its `main`");

		moves = { t3code: { forward: false, onMain: true } };
		const back = await begin(ana, after, { ...after, root: sha("e"), t3code: sha("b") });
		expect(back.status).toBe(403);
		expect(back.body.error).toContain("back or sideways");
	});

	it("hands its maintainers write passwords for a child moved off its main, and not for one moved back", async () => {
		moves = { t3code: { forward: true, onMain: false } };
		const off = await begin(jack, main, { root: sha("c"), t3code: sha("d") });
		expect(off.status).toBe(200);
		expect(off.body.credentials.map((c: any) => c.repo)).toEqual([`jack.${name}.t.root`, `jack.${name}.t.t3code`]);
		await call(jack, "PUT", path("/branches/main"), { base: main, list: { root: sha("c"), t3code: sha("d") } });

		const after = (await call(jack, "GET", path("/branches/main"))).body.list;
		moves = { t3code: { forward: false, onMain: true } };
		const back = await begin(jack, after, { root: sha("e"), t3code: sha("b") });
		expect(back.status).toBe(200);
		expect(back.body.credentials.map((c: any) => c.tracker)).toEqual(["root"]);
	});

	it("without Git to ask, counts every child's move as its maintainers'", async () => {
		const response = await handle(
			new Request(`https://newgit.dev${path("/branches/main/merge")}`, {
				method: "POST",
				headers: { "content-type": "application/json", authorization: `Bearer ${ana}` },
				body: JSON.stringify({ base: main, list: { root: sha("c"), t3code: sha("d") } }),
			}),
			env,
			{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
		);
		expect(response.status).toBe(403);
	});

	it("drops a tracker only its maintainers may, and one no longer defined freely", async () => {
		const dropped = await begin(ana, main, { root: sha("c") });
		expect(dropped.status).toBe(403);
		const byJack = await begin(jack, main, { root: sha("c") });
		expect(byJack.status).toBe(200);
		const finished = await call(jack, "PUT", path("/branches/main"), { base: main, list: { root: sha("c") } });
		expect(finished.status).toBe(200);
		expect(finished.body.list).toEqual({ root: sha("c") });
	});

	it("takes the migration: project dropped, access off its main, the old layout read", async () => {
		// A remote as the old engine left it: `project` in the index,
		// `access`'s files at their workspace paths.
		const old = `old-${crypto.randomUUID().slice(0, 8)}`;
		await call(jack, "POST", "/v1/projects", { name: old, trackers: ["root", "project", "access"] });
		repos.write(
			`jack.${old}.t.access`,
			".newgit/trackers/access.toml",
			'audience = "project-devs"\nstorage = "local"\npaths = [".newgit/trackers"]\ngit = true\nmaintainers = "owner"\n',
		);
		repos.write(
			`jack.${old}.t.access`,
			".newgit/trackers/project.toml",
			'audience = "project-devs"\nstorage = "local"\npaths = [".newgit"]\ngit = true\n',
		);
		const base = { root: sha("1"), project: sha("2"), access: sha("3") };
		const route = (rest: string) => `/v1/projects/jack/${old}${rest}`;
		expect((await call(jack, "PUT", route("/branches/main"), { list: base })).status).toBe(201);
		// The old layout's definitions are read: `access` is the owner's.
		const memberTries = await call(jack, "POST", route("/members"), { login: "ana" });
		expect(memberTries.status).toBe(200);
		moves = { access: { forward: false, onMain: false } };
		const list = { root: sha("4"), access: sha("5") };
		expect((await call(ana, "POST", route("/branches/main/merge"), { base, list })).status).toBe(403);
		// Once migrated, `project` is not defined anywhere: it may only leave.
		repos.write(`jack.${old}.t.access`, ".newgit/trackers/project.toml", "");
		const started = await call(jack, "POST", route("/branches/main/merge"), { base, list });
		expect(started.status).toBe(200);
		expect(started.body.credentials.map((c: any) => c.tracker)).toEqual(["root", "access"]);
		const finished = await call(jack, "PUT", route("/branches/main"), { base, list });
		expect(finished.status).toBe(200);
		expect(finished.body.list).toEqual(list);
	});

	it("takes a tracker new to the branch, its definition landing with the merge, from its governor", async () => {
		// `docs`'s repo was seeded; its definition is not on `main` yet.
		await call(jack, "POST", path("/trackers"), { name: "docs" });
		const list = { ...main, root: sha("c"), docs: sha("d") };
		expect((await begin(ana, main, list)).status).toBe(403);
		const started = await begin(jack, main, list);
		expect(started.status).toBe(200);
		expect(started.body.credentials.map((c: any) => c.tracker)).toEqual(["root"]);
		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list })).status).toBe(200);
		// One without a repo of its own cannot come in.
		const after = (await call(jack, "GET", path("/branches/main"))).body.list;
		expect((await begin(jack, after, { ...after, root: sha("e"), ghost: sha("f") })).status).toBe(400);
	});

	it("writes an index only if root's pushed commit pins exactly that", async () => {
		const list = { root: sha("c"), t3code: sha("d") };
		expect((await begin(ana, main, list)).status).toBe(200);
		// The client checked forward for t3code at `d`, but pushed a root
		// commit pinning it elsewhere.
		pinned = { root: sha("c"), t3code: sha("0") };
		const lied = await call(ana, "PUT", path("/branches/main"), { base: main, list });
		expect(lied.status).toBe(400);
		expect(lied.body.error).toContain("does not pin what this index says");
		expect((await call(ana, "GET", path("/branches/main"))).body.list).toEqual(main);
		pinned = null;
		expect((await call(ana, "PUT", path("/branches/main"), { base: main, list })).status).toBe(200);
	});

	it("needs root in an index, finishing or creating", async () => {
		await begin(jack, main, { ...main, root: sha("c") });
		const rootless = await call(jack, "PUT", path("/branches/main"), { base: main, list: { t3code: sha("c") } });
		expect(rootless.status).toBe(400);
		expect((await call(jack, "PUT", path("/branches/other"), { list: { t3code: sha("c") } })).status).toBe(400);
	});

	it("never carries a tracker outside Git", async () => {
		const refused = await begin(jack, main, { ...main, root: sha("c"), env: sha("e") });
		expect(refused.status).toBe(400);
		expect(refused.body.error).toContain("is not in Git");
	});
});
