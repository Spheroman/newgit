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

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as any };
}

describe("merging into a branch", () => {
	it("hands write passwords for just the trackers that move, then moves the list in one write", async () => {
		const started = await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root"] });
		expect(started.status).toBe(200);
		expect(started.body.credentials.map((c: any) => c.repo)).toEqual([`jack.${name}.t.root`]);
		expect(repos.minted.at(-1)).toMatchObject({ name: `jack.${name}.t.root`, scope: "write" });

		const moved = { ...main, root: sha("c") };
		const finished = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
		expect(finished.status).toBe(200);
		expect((await call(jack, "GET", path("/branches/main"))).body.list).toEqual(moved);
	});

	it("cancels its write passwords when it finishes, and moves nothing it did not declare", async () => {
		const started = await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root"] });
		const minted = repos.minted.at(-1)!.id;
		const sneaky = await call(jack, "PUT", path("/branches/main"), {
			base: main,
			list: { root: sha("c"), t3code: sha("e") },
		});
		expect(sneaky.status).toBe(400);
		expect(sneaky.body.error).toContain("did not declare `t3code`");
		expect(started.status).toBe(200);
		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list: { ...main, root: sha("c") } })).status).toBe(
			200,
		);
		expect(repos.revokedTokens.has(minted)).toBe(true);
	});

	it("is refused with nothing to move", async () => {
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: [] })).status).toBe(400);
	});

	it("refuses a merge onto a list that has moved since", async () => {
		const stale = { ...main, root: sha("f") };
		const refused = await call(jack, "POST", path("/branches/main/merge"), { base: stale, trackers: ["root"] });
		expect(refused.status).toBe(409);
		expect(refused.body.error).toContain("merge again");
	});

	it("lets one merge into a branch at a time", async () => {
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root"] })).status).toBe(
			200,
		);
		const second = await call(ana, "POST", path("/branches/main/merge"), { base: main, trackers: ["root"] });
		expect(second.status).toBe(409);
		expect(second.body.error).toContain("jack is merging");
		// Ana cannot finish a merge she does not hold.
		const sneak = await call(ana, "PUT", path("/branches/main"), { base: main, list: { ...main, root: sha("e") } });
		expect(sneak.status).toBe(409);
		expect((await call(jack, "GET", path("/branches/main"))).body.list).toEqual(main);
	});

	it("is a person's decision: agent tokens are refused", async () => {
		const agent = (await call(jack, "POST", "/v1/tokens", { name: "bot" })).body.token;
		expect((await call(agent, "POST", path("/branches/main/merge"), { base: main, trackers: ["root"] })).status).toBe(
			403,
		);
	});

	it("lets members move root, but only the owner move `access` off its main", async () => {
		const withAccess = { ...main, access: sha("f") };
		await call(jack, "PUT", path("/branches/next"), { list: withAccess });
		expect((await call(ana, "POST", path("/branches/next/merge"), { base: withAccess, trackers: ["root"] })).status).toBe(
			200,
		);
		await call(ana, "PUT", path("/branches/next"), { base: withAccess, list: { ...withAccess, root: sha("1") } });
		const moved = (await call(ana, "GET", path("/branches/next"))).body.list;
		const refused = await call(ana, "POST", path("/branches/next/merge"), {
			base: moved,
			trackers: ["root", "access"],
			off_main: ["access"],
		});
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("project's owner");
	});

	it("needs a child's maintainers only when its pin moves off the child's own main", async () => {
		// ana maintains root, not t3code (jack's alone).
		const off = await call(ana, "POST", path("/branches/main/merge"), {
			base: main,
			trackers: ["root", "t3code"],
			off_main: ["t3code"],
		});
		expect(off.status).toBe(403);
		expect(off.body.error).toContain("t3code");
		const along = await call(ana, "POST", path("/branches/main/merge"), { base: main, trackers: ["root", "t3code"] });
		expect(along.status).toBe(200);
		expect(along.body.credentials.map((c: any) => c.tracker)).toEqual(["root"]);
		const finished = await call(ana, "PUT", path("/branches/main"), {
			base: main,
			list: { root: sha("c"), t3code: sha("d") },
		});
		expect(finished.status).toBe(200);
	});

	it("hands write passwords for root and every tracker off its main", async () => {
		const started = await call(jack, "POST", path("/branches/main/merge"), {
			base: main,
			trackers: ["root", "t3code"],
			off_main: ["t3code"],
		});
		expect(started.status).toBe(200);
		expect(started.body.credentials.map((c: any) => c.repo)).toEqual([`jack.${name}.t.root`, `jack.${name}.t.t3code`]);
	});

	it("may drop a tracker it declared from the index, and only one it declared", async () => {
		await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root"] });
		const undeclared = await call(jack, "PUT", path("/branches/main"), { base: main, list: { root: sha("c") } });
		expect(undeclared.status).toBe(400);
		expect(undeclared.body.error).toContain("cannot drop");

		await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root", "t3code"] });
		const dropped = await call(jack, "PUT", path("/branches/main"), { base: main, list: { root: sha("c") } });
		expect(dropped.status).toBe(200);
		expect(dropped.body.list).toEqual({ root: sha("c") });
	});

	it("refuses a merge that does not move root, and an index without root", async () => {
		const noRoot = await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["t3code"] });
		expect(noRoot.status).toBe(400);
		await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root", "t3code"] });
		const rootless = await call(jack, "PUT", path("/branches/main"), { base: main, list: { t3code: sha("c") } });
		expect(rootless.status).toBe(400);
		expect(rootless.body.error).toContain("`root`");
		expect((await call(jack, "PUT", path("/branches/other"), { list: { t3code: sha("c") } })).status).toBe(400);
	});

	it("never carries a tracker outside Git", async () => {
		const refused = await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["root", "env"] });
		expect(refused.status).toBe(400);
		expect(refused.body.error).toContain("is not in Git");
	});
});
