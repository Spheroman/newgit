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
const main = { root: sha("a"), project: sha("b") };

beforeEach(async () => {
	repos = new FakeRepos();
	name = `web-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "project"] });
	const defs = `jack.${name}.t.access`;
	repos.write(defs, ".newgit/trackers/root.toml", 'audience = "project-devs"\nsecret = false\n');
	repos.write(defs, ".newgit/trackers/project.toml", 'audience = "project-devs"\nsecret = false\n');
	repos.write(defs, ".newgit/trackers/env.toml", 'audience = "project-devs"\n');
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
			list: { root: sha("c"), project: sha("e") },
		});
		expect(sneaky.status).toBe(400);
		expect(sneaky.body.error).toContain("did not declare `project`");
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

	it("lets members change resources and scripts, but only the owner who may read what", async () => {
		const withDefinitions = { ...main, access: sha("f") };
		await call(jack, "PUT", path("/branches/next"), { list: withDefinitions });
		expect(
			(await call(ana, "POST", path("/branches/next/merge"), { base: withDefinitions, trackers: ["project"] })).status,
		).toBe(200);
		await call(ana, "PUT", path("/branches/next"), { base: withDefinitions, list: { ...withDefinitions, project: sha("1") } });
		const moved = (await call(ana, "GET", path("/branches/next"))).body.list;
		const refused = await call(ana, "POST", path("/branches/next/merge"), { base: moved, trackers: ["access"] });
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("project's owner");
	});

	it("never carries a secret tracker", async () => {
		const refused = await call(jack, "POST", path("/branches/main/merge"), { base: main, trackers: ["env"] });
		expect(refused.status).toBe(400);
		expect(refused.body.error).toContain("secret tracker");
	});
});
