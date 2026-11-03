import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

// The guardrails that make `ALLOWED_LOGINS = "*"` safe to run: a cap on
// projects per login, a blocklist, and admins who can take anything down.

const login = async (who: string) => (await issue(env, { kind: "login", login: who, ttlSeconds: LOGIN_TTL_SECONDS })).token;

async function call(
	settings: Partial<Env>,
	token: string,
	method: string,
	path: string,
	body?: unknown,
) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		{ ...env, ALLOWED_LOGINS: "*", ...settings },
		{ repos: new FakeRepos(), fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as any };
}

const unique = (prefix: string) => `${prefix}-${crypto.randomUUID().slice(0, 8)}`;
const create = (settings: Partial<Env>, token: string, name: string) =>
	call(settings, token, "POST", "/v1/projects", { name, trackers: ["root"] });

describe("the project cap", () => {
	it("stops a login at its cap, and deleting one makes room", async () => {
		const who = unique("capped");
		const token = await login(who);
		const settings = { PROJECTS_PER_LOGIN: "2" };
		const [a, b, c] = [unique("a"), unique("b"), unique("c")];
		expect((await create(settings, token, a)).status).toBe(201);
		expect((await create(settings, token, b)).status).toBe(201);
		const refused = await create(settings, token, c);
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("already owns 2 projects");

		expect((await call(settings, token, "DELETE", `/v1/projects/${who}/${a}`)).status).toBe(200);
		expect((await create(settings, token, c)).status).toBe(201);
	});

	it("still answers 409 for a name the login already has, at the cap", async () => {
		const token = await login(unique("full"));
		const settings = { PROJECTS_PER_LOGIN: "1" };
		const name = unique("only");
		expect((await create(settings, token, name)).status).toBe(201);
		expect((await create(settings, token, name)).status).toBe(409);
	});

	it("defaults to 5", async () => {
		const token = await login(unique("default"));
		for (let i = 0; i < 5; i++) expect((await create({ PROJECTS_PER_LOGIN: undefined }, token, unique("p"))).status).toBe(201);
		expect((await create({ PROJECTS_PER_LOGIN: undefined }, token, unique("p"))).status).toBe(403);
	});

	it("does not bind admins", async () => {
		const who = unique("admin");
		const token = await login(who);
		const settings = { PROJECTS_PER_LOGIN: "0", ADMIN_LOGINS: who };
		expect((await create(settings, token, unique("x"))).status).toBe(201);
	});
});

describe("the blocklist", () => {
	it("shuts out a blocked login even when everyone is allowed", async () => {
		const who = unique("eve");
		const token = await login(who);
		expect((await call({}, token, "GET", "/v1/me")).status).toBe(200);
		const refused = await call({ BLOCKED_LOGINS: `someone, ${who}` }, token, "GET", "/v1/me");
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("is blocked");
	});
});

describe("admins", () => {
	it("may delete a project they do not own; nobody else may", async () => {
		const owner = unique("owner");
		const [ownerToken, admin, stranger] = await Promise.all([login(owner), login("op"), login(unique("stranger"))]);
		const name = unique("spam");
		const settings = { ADMIN_LOGINS: "op" };
		expect((await create(settings, ownerToken, name)).status).toBe(201);
		expect((await call(settings, stranger, "DELETE", `/v1/projects/${owner}/${name}`)).status).toBe(403);
		expect((await call(settings, admin, "DELETE", `/v1/projects/${owner}/${name}`)).status).toBe(200);
		expect((await call(settings, ownerToken, "GET", `/v1/projects/${owner}/${name}`)).status).toBe(404);
	});
});
