import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

let repos: FakeRepos;
let slug: string;
let project: string;
let jack: string;
let ana: string;

const login = async (who: string) => (await issue(env, { kind: "login", login: who, ttlSeconds: LOGIN_TTL_SECONDS })).token;
const repo = (suffix: string) => `jack.${slug}.${suffix}`;

beforeEach(async () => {
	repos = new FakeRepos();
	slug = `acme-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
	[jack, ana] = await Promise.all([login("jack"), login("ana")]);
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project"] })).status).toBe(201);
	expect((await call(jack, "POST", `/v1/projects/${project}/members`, { login: "ana" })).status).toBe(200);
	repos.write(repo("t.access"), ".newgit/trackers/root.toml", 'audience = "project-devs"\nsecret = false\n');
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

describe("deleting a project", () => {
	let instanceToken: string;

	beforeEach(async () => {
		const spawned = await call(ana, "POST", `/v1/projects/${project}/instances`, { name: "fix", base_rev: "abc" });
		expect(spawned.status).toBe(201);
		instanceToken = spawned.body.token;
		expect((await call(ana, "POST", `/v1/projects/${project}/instances/fix/trackers/root/fork`)).status).toBe(200);
		// Another project whose name starts the same way keeps its repos.
		repos.repos.set(`jack.${slug}x.t.root`, new Map());
	});

	it("is the owner's alone, and never an agent's", async () => {
		expect((await call(ana, "DELETE", `/v1/projects/${project}`)).status).toBe(403);
		expect((await call(instanceToken, "DELETE", `/v1/projects/${project}`)).status).toBe(403);
		expect((await call(jack, "GET", `/v1/projects/${project}`)).status).toBe(200);
	});

	it("takes its repos, its record, and every token pinned to it", async () => {
		const deleted = await call(jack, "DELETE", `/v1/projects/${project}`);
		expect(deleted.status).toBe(200);
		expect(deleted.body.repos_deleted).toBe(4);
		expect([...repos.repos.keys()]).toEqual([`jack.${slug}x.t.root`]);
		expect((await call(jack, "GET", `/v1/projects/${project}`)).status).toBe(404);
		expect((await call(instanceToken, "GET", `/v1/projects/${project}/instances/fix`)).status).toBe(401);
	});

	it("frees the name to be created again, from scratch", async () => {
		expect((await call(jack, "DELETE", `/v1/projects/${project}`)).status).toBe(200);
		expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root"] })).status).toBe(201);
		const view = await call(jack, "GET", `/v1/projects/${project}`);
		expect(view.body.members.map((m: any) => m.login)).toEqual(["jack"]);
		expect((await call(jack, "GET", `/v1/projects/${project}/instances`)).body.instances).toEqual([]);
	});
});
