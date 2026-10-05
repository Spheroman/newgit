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
let eve: string;

const login = async (who: string) => (await issue(env, { kind: "login", login: who, ttlSeconds: LOGIN_TTL_SECONDS })).token;

beforeEach(async () => {
	repos = new FakeRepos();
	slug = `acme-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
	[jack, ana, eve] = await Promise.all([login("jack"), login("ana"), login("eve")]);
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project"] })).status).toBe(201);
	expect((await call(jack, "POST", `/v1/projects/${project}/members`, { login: "ana" })).status).toBe(200);
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

const commit = (digit: string) => digit.repeat(40);
const put = (token: string, branch: string, list: unknown) =>
	call(token, "PUT", `/v1/projects/${project}/branches/${branch}`, { list });

describe("branches", () => {
	it("start empty, with main as the default", async () => {
		expect((await call(jack, "GET", `/v1/projects/${project}/branches`)).body).toEqual({ default: "main", branches: [] });
	});

	it("are created from a list of one commit per tracker, and read back", async () => {
		const list = { root: commit("a"), project: commit("b") };
		const created = await put(ana, "main", list);
		expect(created.status).toBe(201);
		expect(created.body).toMatchObject({ name: "main", list, createdBy: "ana" });
		expect((await call(jack, "GET", `/v1/projects/${project}/branches/main`)).body).toMatchObject({ name: "main", list });
		const listed = await call(jack, "GET", `/v1/projects/${project}/branches`);
		expect(listed.body.branches.map((b: { name: string }) => b.name)).toEqual(["main"]);
		expect((await call(jack, "GET", `/v1/projects/${project}`)).body.branches).toHaveLength(1);
	});

	it("are written once: a second write is a conflict and changes nothing", async () => {
		await put(jack, "main", { root: commit("a") });
		const second = await put(jack, "main", { root: commit("c") });
		expect(second.status).toBe(409);
		expect(second.body.error).toContain("newgit merge");
		expect((await call(jack, "GET", `/v1/projects/${project}/branches/main`)).body.list).toEqual({ root: commit("a") });
	});

	it("have to be named something a repo part could be, with a commit for every tracker", async () => {
		for (const list of [undefined, null, {}, { root: "main" }, { root: commit("a").slice(1) }, { root: commit("A") }, { root: 7 }, { "r.t": commit("a") }])
			expect((await put(jack, "feature", list)).status, JSON.stringify(list)).toBe(400);
		expect((await put(jack, "a.b", { root: commit("a") })).status).toBe(400);
		expect((await call(jack, "GET", `/v1/projects/${project}/branches`)).body.branches).toEqual([]);
	});

	it("say 404 for one that does not exist", async () => {
		expect((await call(jack, "GET", `/v1/projects/${project}/branches/ghost`)).status).toBe(404);
	});

	it("are for members only", async () => {
		expect((await put(eve, "main", { root: commit("a") })).status).toBe(403);
		expect((await call(eve, "GET", `/v1/projects/${project}/branches`)).status).toBe(403);
	});

	it("change the default only by the owner, and only to one that exists", async () => {
		await put(jack, "main", { root: commit("a") });
		await put(jack, "next", { root: commit("b") });
		const setDefault = (token: string, name: string) =>
			call(token, "PUT", `/v1/projects/${project}/branches/default`, { name });
		expect((await setDefault(ana, "next")).status).toBe(403);
		expect((await setDefault(jack, "ghost")).status).toBe(404);
		expect((await call(jack, "PUT", `/v1/projects/${project}/branches/default`, {})).status).toBe(400);
		expect((await setDefault(jack, "next")).body).toEqual({ default: "next" });
		expect((await call(ana, "GET", `/v1/projects/${project}/branches`)).body.default).toBe("next");
	});

	it("are refused to instance tokens, which also cannot move the default", async () => {
		await put(jack, "main", { root: commit("a") });
		const spawned = (await call(jack, "POST", `/v1/projects/${project}/instances`, { name: "attempt-1", base_rev: "abc" })).body;
		const refused = await put(spawned.token, "sneaky", { root: commit("c") });
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("agent tokens cannot");
		expect((await call(spawned.token, "PUT", `/v1/projects/${project}/branches/default`, { name: "main" })).status).toBe(403);
		expect((await call(spawned.token, "GET", `/v1/projects/${project}/branches/sneaky`)).status).toBe(404);
		// Reading is fine.
		expect((await call(spawned.token, "GET", `/v1/projects/${project}/branches/main`)).status).toBe(200);
	});
});
