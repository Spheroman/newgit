import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

let repos: FakeRepos;
let pending: Promise<unknown>[];
let project: string;
let slug: string;
let jack: string;

beforeEach(async () => {
	repos = new FakeRepos();
	pending = [];
	slug = `acme-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project"] })).status).toBe(201);
	const define = (name: string, text: string) => repos.write(`jack.${slug}.t.access`, `.newgit/trackers/${name}.toml`, text);
	define("root", 'audience = "project-devs"\nsecret = false\n');
	define("runtime-env", 'audience = "user"\n');
});

async function call(token: string, method: string, path: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: (promise) => pending.push(promise), endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as any };
}

/** Spawn, as the CLI does: the response carries the workspace's own token. */
async function spawn(name: string, as = jack) {
	const created = await call(as, "POST", `/v1/projects/${project}/instances`, { name, base_rev: "abc" });
	expect(created.status).toBe(201);
	await Promise.all(pending);
	return created.body as { token: string; token_id: string };
}

const credential = (token: string, repo: string, scope = "read") =>
	call(token, "POST", `/v1/projects/${project}/credentials`, { repo, scope });

describe("instance tokens", () => {
	it("are minted by spawn, with nothing for anyone to create by hand", async () => {
		const attempt = await spawn("attempt-1");
		expect(attempt.token.startsWith("ngt_")).toBe(true);
		expect((await call(attempt.token, "GET", "/v1/me")).body).toMatchObject({
			login: "jack",
			kind: "agent",
			project,
		});
		expect((await call(attempt.token, "GET", "/v1/me")).body.agent).toBeUndefined();
	});

	it("write their own instance and nothing else", async () => {
		const one = await spawn("attempt-1");
		await spawn("attempt-2");

		for (const name of ["attempt-1", "attempt-2"])
			expect((await call(jack, "POST", `/v1/projects/${project}/instances/${name}/trackers/root/fork`)).status).toBe(200);
		expect((await credential(one.token, `jack.${slug}.i.attempt-1.root`, "write")).status).toBe(200);
		expect((await call(one.token, "PUT", `/v1/projects/${project}/checkpoints/attempt-1/ckpt_001`, { record: {} })).status).toBe(201);
		expect((await call(one.token, "PUT", `/v1/projects/${project}/instances/attempt-1/head`, { head_rev: "def" })).status).toBe(200);

		// Acting as jack, it may still read the code tracker and use jack's own lanes.
		expect((await credential(one.token, `jack.${slug}.t.root`)).status).toBe(200);
		expect((await credential(one.token, `jack.${slug}.t.runtime-env.jack`, "write")).status).toBe(200);

		// But not another attempt, even one jack owns.
		for (const refused of [
			await credential(one.token, `jack.${slug}.i.attempt-2.root`, "write"),
			await call(one.token, "PUT", `/v1/projects/${project}/checkpoints/attempt-2/ckpt_001`, { record: {} }),
			await call(one.token, "PUT", `/v1/projects/${project}/instances/attempt-2/head`, { head_rev: "def" }),
		]) {
			expect(refused.status).toBe(403);
			expect(refused.body.error).toContain("`attempt-1` only");
		}
	});

	it("cannot create or remove instances, mint tokens, or manage members", async () => {
		const one = await spawn("attempt-1");
		expect((await call(one.token, "POST", `/v1/projects/${project}/instances`, { name: "x", base_rev: "a" })).status).toBe(403);
		expect((await call(one.token, "DELETE", `/v1/projects/${project}/instances/attempt-1`)).status).toBe(403);
		expect((await call(one.token, "POST", "/v1/tokens", { name: "child" })).status).toBe(403);
		expect((await call(one.token, "POST", `/v1/projects/${project}/members`, { login: "eve" })).status).toBe(403);
	});

	it("renew into a fresh token and revoke the copy they replaced", async () => {
		const one = await spawn("attempt-1");
		const renewed = await call(one.token, "POST", "/v1/tokens/renew");
		expect(renewed.status).toBe(201);
		expect(renewed.body).toMatchObject({ instance: "attempt-1", project });
		expect((await call(renewed.body.token, "GET", "/v1/me")).status).toBe(200);
		expect((await call(one.token, "GET", "/v1/me")).status).toBe(401);
	});

	it("are revoked when the instance is removed, and leave other attempts working", async () => {
		const one = await spawn("attempt-1");
		const two = await spawn("attempt-2");
		expect((await call(jack, "DELETE", `/v1/projects/${project}/instances/attempt-1`)).status).toBe(200);
		expect((await call(one.token, "GET", "/v1/me")).status).toBe(401);
		expect((await call(two.token, "GET", "/v1/me")).status).toBe(200);
	});

	it("an instance spawned by an agent is named for that agent", async () => {
		const agent = (await call(jack, "POST", "/v1/tokens", { name: "orchestrator" })).body.token;
		const spawned = await spawn("attempt-1", agent);
		expect((await call(spawned.token, "GET", "/v1/me")).body.agent).toBe("orchestrator");
	});
});
