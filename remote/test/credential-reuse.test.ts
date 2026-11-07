import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, fakeCopy, noEndpoint } from "./fake-repos";

// The credentials route hands a token the password it minted for that repo
// and scope until it nears expiring, as the Git endpoint does: a checkpoint
// asks once per tracker it syncs. What the caller may do is still decided
// every time, before the reused password is looked at.

let repos: FakeRepos;
let slug: string;
let project: string;
let jack: string;

beforeEach(async () => {
	repos = new FakeRepos();
	slug = `acme-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access"] })).status).toBe(201);
	repos.write(`jack.${slug}.t.access`, "root.toml", 'audience = "project-devs"\ngit = true\n');
});

async function call(token: string, method: string, path: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint, copy: fakeCopy },
	);
	return { status: response.status, body: (await response.json()) as any };
}

const ask = (token: string, scope = "read") =>
	call(token, "POST", `/v1/projects/${project}/credentials`, { repo: `jack.${slug}.t.root`, scope });

describe("the credentials route", () => {
	it("hands a token the password it minted already", async () => {
		const first = await ask(jack);
		const minted = repos.minted.length;
		const again = await ask(jack);
		expect(again.status).toBe(200);
		expect(again.body.password).toBe(first.body.password);
		expect(repos.minted.length).toBe(minted);
	});

	it("mints for another token", async () => {
		await ask(jack);
		const other = (await call(jack, "POST", "/v1/tokens", { name: "agent-1" })).body;
		const minted = repos.minted.length;
		expect((await ask(other.token)).status).toBe(200);
		expect(repos.minted.length).toBe(minted + 1);
	});

	it("refuses a revoked token, whatever was minted for it", async () => {
		const agent = (await call(jack, "POST", "/v1/tokens", { name: "agent-2" })).body;
		expect((await ask(agent.token)).status).toBe(200);
		expect((await call(jack, "DELETE", `/v1/tokens/${agent.id}`)).status).toBe(200);
		expect((await ask(agent.token)).status).toBe(401);
	});

	it("refuses once the audience narrows, whatever was minted", async () => {
		await call(jack, "POST", `/v1/projects/${project}/members`, { login: "ana" });
		const ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		expect((await ask(ana, "read")).status).toBe(200);
		repos.write(`jack.${slug}.t.access`, "root.toml", 'audience = "user:jack"\ngit = true\n');
		expect((await ask(ana, "read")).status).toBe(403);
	});
});
