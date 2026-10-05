import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import type { EndpointKind, EndpointPlan } from "../src/endpoint";
import { handle, issue } from "../src/index";
import { FakeRepos } from "./fake-repos";

let repos: FakeRepos;
let name: string;
let jack: { token: string; id: string };
let ana: string;
let served: { project: string; kind: EndpointKind; plan: EndpointPlan }[];

const sha = (c: string) => c.repeat(40);

beforeEach(async () => {
	repos = new FakeRepos();
	served = [];
	name = `web-${crypto.randomUUID().slice(0, 8)}`;
	jack = await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS });
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect(
		(await api(jack.token, "POST", "/v1/projects", { name, trackers: ["root", "access", "project", "payments"] })).status,
	).toBe(201);
	const defs = `jack.${name}.t.access`;
	repos.write(defs, ".newgit/trackers/root.toml", 'audience = "project-devs"\nsecret = false\n');
	repos.write(defs, ".newgit/trackers/project.toml", 'audience = "project-devs"\nsecret = false\n');
	// A code tracker only Jack may read, and a secret tracker: neither is Ana's.
	repos.write(defs, ".newgit/trackers/payments.toml", 'audience = "user:jack"\nsecret = false\n');
	repos.write(defs, ".newgit/trackers/env.toml", 'audience = "project-devs"\n');
	expect(
		(
			await api(jack.token, "PUT", `/v1/projects/jack/${name}/branches/main`, {
				list: { root: sha("a"), project: sha("b"), payments: sha("c"), env: sha("d"), access: sha("e") },
			})
		).status,
	).toBe(201);
	expect((await api(jack.token, "POST", `/v1/projects/jack/${name}/members`, { login: "ana" })).status).toBe(200);
});

const deps = () => ({
	repos,
	fetcher: fetch,
	waitUntil: () => {},
	endpoint: async (project: string, kind: EndpointKind, plan: EndpointPlan) => {
		served.push({ project, kind, plan });
		return new Response("served");
	},
});

async function api(token: string, method: string, path: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		deps(),
	);
	return { status: response.status, body: (await response.json()) as any };
}

/** What Git sends: Basic auth, the token as the password. */
async function git(token: string | null, path: string, method = "GET") {
	const headers: Record<string, string> = {};
	if (token !== null) headers.authorization = `Basic ${btoa(`x:${token}`)}`;
	return handle(new Request(`https://newgit.dev/jack/${name}.git${path}`, { method, headers }), env, deps());
}

describe("the Git endpoint", () => {
	it("asks Git for credentials when it has none", async () => {
		const response = await git(null, "/info/refs?service=git-upload-pack");
		expect(response.status).toBe(401);
		expect(response.headers.get("www-authenticate")).toContain("Basic");
		expect(await response.text()).toContain("newgit login");
	});

	it("is read-only: a push goes through a newgit workspace", async () => {
		const response = await git(jack.token, "/info/refs?service=git-receive-pack");
		expect(response.status).toBe(403);
		expect(await response.text()).toContain("newgit workspace");
	});

	it("composes every code tracker its reader may read, and nothing else", async () => {
		expect((await git(jack.token, "/info/refs?service=git-upload-pack")).status).toBe(200);
		expect((await git(jack.token, "/git-upload-pack", "POST")).status).toBe(200);
		expect(served.map((call) => call.kind)).toEqual(["advertise", "upload-pack"]);
		const plan = served[0].plan;
		expect(served[0].project).toBe(`jack/${name}`);
		expect(plan.branches.main).toEqual({ root: sha("a"), project: sha("b"), payments: sha("c"), access: sha("e") });
		expect(plan.definitions).toEqual({ access: sha("e") });
		expect(Object.keys(plan.repos).sort()).toEqual(["access", "payments", "project", "root"]);
		expect(plan.repos.root.url).toBe(repos.url(`jack.${name}.t.root`));
		expect(plan.repos.root.auth.startsWith("Authorization: Basic ")).toBe(true);
	});

	it("leaves out a tracker its reader may not read — the secret one never enters", async () => {
		expect((await git(ana, "/info/refs?service=git-upload-pack")).status).toBe(200);
		const plan = served[0].plan;
		expect(plan.branches.main).toEqual({ root: sha("a"), project: sha("b"), access: sha("e") });
		expect(plan.repos.payments).toBeUndefined();
		expect(plan.repos.env).toBeUndefined();
	});

	it("is closed to someone who is not a member", async () => {
		const eve = (await issue(env, { kind: "login", login: "eve", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		expect((await git(eve, "/info/refs?service=git-upload-pack")).status).toBe(404);
		expect(served).toEqual([]);
	});

	it("mints its read passwords against the reader's token, so revoking it revokes them", async () => {
		await git(jack.token, "/info/refs?service=git-upload-pack");
		const minted = repos.minted.filter((token) => token.scope === "read").map((token) => token.id);
		expect(minted.length).toBe(4);
		expect((await api(jack.token, "DELETE", `/v1/tokens/${jack.id}`)).status).toBe(200);
		for (const id of minted) expect(repos.revokedTokens.has(id)).toBe(true);
		expect((await git(jack.token, "/info/refs?service=git-upload-pack")).status).toBe(401);
	});
});
