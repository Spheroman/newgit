import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS, signToken } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

let repos: FakeRepos;
let project: string;
let slug: string;
let jack: string;

beforeEach(async () => {
	repos = new FakeRepos();
	slug = `acme-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project"] })).status).toBe(201);
});

async function call(token: string, method: string, path: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as any };
}

async function agentToken(name: string, extra: Record<string, unknown> = {}) {
	const created = await call(jack, "POST", "/v1/tokens", { name, ...extra });
	expect(created.status).toBe(201);
	return created.body as { token: string; id: string; expiresAt: string };
}

describe("tokens", () => {
	it("are good only while recorded: a correctly signed token nobody issued is refused", async () => {
		const { token } = await signToken(env.TOKEN_KEY, { kind: "login", login: "jack", ttlSeconds: 60 });
		const refused = await call(token, "GET", "/v1/me");
		expect(refused.status).toBe(401);
		expect(refused.body.error).toContain("revoked");
	});

	it("an agent acts as its owner, and records say which agent", async () => {
		const agent = await agentToken("agent-3");
		expect((await call(agent.token, "GET", "/v1/me")).body).toMatchObject({
			login: "jack",
			kind: "agent",
			agent: "agent-3",
		});
		const instance = await call(agent.token, "POST", `/v1/projects/${project}/instances`, {
			name: "attempt-1",
			base_rev: "abc",
		});
		expect(instance.body).toMatchObject({ owner: "jack", agent: "agent-3" });
		const checkpoint = await call(agent.token, "PUT", `/v1/projects/${project}/checkpoints/attempt-1/ckpt_001`, {
			record: {},
		});
		expect(checkpoint.body).toMatchObject({ createdBy: "jack", agent: "agent-3" });
	});

	it("an agent defaults to 7 days, and may not ask for more than 30", async () => {
		const agent = await agentToken("agent-3");
		const days = (Date.parse(agent.expiresAt) - Date.now()) / 86_400_000;
		expect(days).toBeGreaterThan(6.9);
		expect(days).toBeLessThan(7.1);
		expect((await call(jack, "POST", "/v1/tokens", { name: "greedy", ttl_days: 31 })).status).toBe(400);
	});

	it("an agent cannot mint tokens, list them, create projects, or manage members", async () => {
		const agent = await agentToken("agent-3");
		for (const [method, path, body] of [
			["POST", "/v1/tokens", { name: "child" }],
			["GET", "/v1/tokens", undefined],
			["POST", "/v1/projects", { name: `${slug}-2`, trackers: ["root"] }],
			["POST", `/v1/projects/${project}/members`, { login: "eve" }],
		] as const) {
			const refused = await call(agent.token, method, path, body);
			expect(refused.status, `${method} ${path}`).toBe(403);
			expect(refused.body.error).toContain("agent tokens cannot");
		}
	});

	it("an agent pinned to a project cannot touch another", async () => {
		const other = `jack/${slug}-other`;
		expect((await call(jack, "POST", "/v1/projects", { name: `${slug}-other`, trackers: ["root"] })).status).toBe(201);
		const pinned = await agentToken("agent-3", { project });
		expect((await call(pinned.token, "GET", `/v1/projects/${project}`)).status).toBe(200);
		const refused = await call(pinned.token, "GET", `/v1/projects/${other}`);
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain(`for ${project} only`);
	});

	it("revoking one token stops it at once and leaves every other token working", async () => {
		const leaked = await agentToken("agent-3");
		const other = await agentToken("agent-4");
		expect((await call(leaked.token, "GET", "/v1/me")).status).toBe(200);

		expect((await call(jack, "DELETE", `/v1/tokens/${leaked.id}`)).body).toMatchObject({ id: leaked.id, revoked: true });
		expect((await call(leaked.token, "GET", "/v1/me")).status).toBe(401);
		expect((await call(other.token, "GET", "/v1/me")).status).toBe(200);
		expect((await call(jack, "GET", "/v1/me")).status).toBe(200);

		const listed = await call(jack, "GET", "/v1/tokens");
		const row = listed.body.tokens.find((token: { id: string }) => token.id === leaked.id);
		expect(row.revokedAt).not.toBeNull();
	});

	it("an agent may revoke itself, and nothing else", async () => {
		const agent = await agentToken("agent-3");
		const other = await agentToken("agent-4");
		expect((await call(agent.token, "DELETE", `/v1/tokens/${other.id}`)).status).toBe(403);
		expect((await call(agent.token, "DELETE", `/v1/tokens/${agent.id}`)).status).toBe(200);
		expect((await call(agent.token, "GET", "/v1/me")).status).toBe(401);
		expect((await call(other.token, "GET", "/v1/me")).status).toBe(200);
	});

	it("a human can revoke their own login", async () => {
		const me = await call(jack, "GET", "/v1/me");
		expect((await call(jack, "DELETE", `/v1/tokens/${me.body.token_id}`)).status).toBe(200);
		expect((await call(jack, "GET", "/v1/me")).status).toBe(401);
	});
});
