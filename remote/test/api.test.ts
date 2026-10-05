import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { issueToken } from "../src/auth";
import { handle } from "../src/index";
import { FakeRepos } from "./fake-repos";

let repos: FakeRepos;
let pending: Promise<unknown>[];
let project: string;

beforeEach(() => {
	repos = new FakeRepos();
	pending = [];
	// Durable Object storage persists across tests in a file; a fresh name
	// per test keeps them independent.
	project = `acme-${crypto.randomUUID().slice(0, 8)}`;
});

async function call(login: string | null, method: string, path: string, body?: unknown) {
	const headers: Record<string, string> = { "content-type": "application/json" };
	if (login) headers.authorization = `Bearer ${await issueToken(env.TOKEN_KEY, login)}`;
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers,
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: (promise) => pending.push(promise) },
	);
	return { status: response.status, body: (await response.json()) as any };
}

async function setUp() {
	expect((await call("jack", "POST", "/v1/projects", { name: project })).status).toBe(201);
	expect((await call("jack", "POST", `/v1/projects/${project}/members`, { login: "Ana" })).status).toBe(200);
	// Trunk's committed tracker definitions are what audiences are read from.
	repos.write(`${project}.src`, ".newgit/trackers/runtime-env.toml", 'audience = "user"\nstorage = "remote"\n');
	repos.write(`${project}.src`, ".newgit/trackers/jack-env.toml", 'audience = "user:jack"\nstorage = "remote"\n');
	repos.write(`${project}.src`, ".newgit/trackers/db-snapshots.toml", 'audience = "project-devs"\nstorage = "remote"\n');
}

const credential = (login: string, repo: string, scope = "read") =>
	call(login, "POST", `/v1/projects/${project}/credentials`, { repo, scope });

describe("the API", () => {
	it("needs a token everywhere but login", async () => {
		expect((await call(null, "GET", "/v1/me")).status).toBe(401);
		expect((await call("jack", "GET", "/v1/me")).body).toEqual({ login: "jack" });
	});

	it("creates a project with trunk, and refuses a second with the same name", async () => {
		const created = await call("jack", "POST", "/v1/projects", { name: project });
		expect(created.body.src.repo).toBe(`${project}.src`);
		// The creator's one-time write to trunk, for the initial push.
		expect(created.body.seed_credential.password).toBe(`pw-${project}.src-write`);
		expect(repos.repos.has(`${project}.src`)).toBe(true);
		expect((await call("ana", "POST", "/v1/projects", { name: project })).status).toBe(409);
	});

	it("keeps non-members out and lets only the owner manage members", async () => {
		await setUp();
		expect((await call("eve", "GET", `/v1/projects/${project}`)).status).toBe(403);
		const info = await call("ana", "GET", `/v1/projects/${project}`);
		expect(info.body.members.map((m: { login: string }) => m.login)).toEqual(["ana", "jack"]);
		expect((await call("ana", "POST", `/v1/projects/${project}/members`, { login: "eve" })).status).toBe(403);
	});

	it("records an instance at once and forks it in the background", async () => {
		await setUp();
		const created = await call("ana", "POST", `/v1/projects/${project}/instances`, {
			name: "fix-login",
			base_rev: "abc123",
		});
		expect(created.status).toBe(201);
		expect(created.body).toMatchObject({ owner: "ana", forkState: "pending", repo: `${project}.i.fix-login` });
		await Promise.all(pending);
		const after = await call("ana", "GET", `/v1/projects/${project}/instances/fix-login`);
		expect(after.body.forkState).toBe("ready");
		expect(repos.repos.has(`${project}.i.fix-login`)).toBe(true);
	});

	it("mints credentials by the rules, and says why when it will not", async () => {
		await setUp();
		await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "abc" });
		await Promise.all(pending);

		// Trunk: readable, never writable.
		expect((await credential("jack", `${project}.src`)).body.password).toBe(`pw-${project}.src-read`);
		expect((await credential("jack", `${project}.src`, "write")).status).toBe(403);

		// A fork: only its owner pushes. A credential helper passes a URL.
		const url = `https://acct.artifacts.example/git/newgit/${project}.i.fix-login.git`;
		expect((await credential("ana", url, "write")).status).toBe(200);
		const refused = await credential("jack", url, "write");
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("belongs to ana");

		// A per-user tracker: your own repo only, created on first write.
		expect((await credential("jack", `${project}.t.runtime-env.jack`, "read")).status).toBe(404);
		expect((await credential("jack", `${project}.t.runtime-env.jack`, "write")).status).toBe(200);
		expect(repos.repos.has(`${project}.t.runtime-env.jack`)).toBe(true);
		expect((await credential("ana", `${project}.t.runtime-env.jack`)).status).toBe(403);

		// A named tracker: refused to everyone else, with the reason.
		await credential("jack", `${project}.t.jack-env`, "write");
		const named = await credential("ana", `${project}.t.jack-env`);
		expect(named.status).toBe(403);
		expect(named.body.error).toContain("user:jack");

		// Outsiders get nothing from a project-devs tracker.
		await credential("jack", `${project}.t.db-snapshots`, "write");
		expect((await credential("eve", `${project}.t.db-snapshots`)).status).toBe(403);

		// A tracker trunk does not define, and another project's repo.
		expect((await credential("jack", `${project}.t.ghost`, "write")).status).toBe(403);
		expect((await credential("jack", "other.src")).status).toBe(400);
	});

	it("takes an audience change only from trunk", async () => {
		await setUp();
		await credential("jack", `${project}.t.jack-env`, "write");
		expect((await credential("ana", `${project}.t.jack-env`)).status).toBe(403);
		// A merged definition change is what opens it up.
		repos.write(`${project}.src`, ".newgit/trackers/jack-env.toml", 'audience = "project-devs"\n');
		expect((await credential("ana", `${project}.t.jack-env`)).status).toBe(200);
	});

	it("stores checkpoints for the instance owner, readable by members", async () => {
		await setUp();
		await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "abc" });
		const record = { id: "ckpt_001", tracker_states: [{ name: "runtime-env", content_rev: "fc4528d24088" }] };
		expect((await call("jack", "PUT", `/v1/projects/${project}/checkpoints/fix-login/ckpt_001`, { record })).status).toBe(403);
		expect((await call("ana", "PUT", `/v1/projects/${project}/checkpoints/fix-login/ckpt_001`, { record })).status).toBe(201);
		const read = await call("jack", "GET", `/v1/projects/${project}/checkpoints/fix-login/ckpt_001`);
		expect(read.body).toMatchObject({ createdBy: "ana", record });
		const list = await call("jack", "GET", `/v1/projects/${project}/checkpoints?instance=fix-login`);
		expect(list.body.checkpoints).toHaveLength(1);
	});

	it("removes an instance with its fork and its checkpoints", async () => {
		await setUp();
		await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "abc" });
		await Promise.all(pending);
		await call("ana", "PUT", `/v1/projects/${project}/checkpoints/fix-login/ckpt_001`, { record: {} });
		expect((await call("jack", "DELETE", `/v1/projects/${project}/instances/fix-login`)).status).toBe(403);
		expect((await call("ana", "DELETE", `/v1/projects/${project}/instances/fix-login`)).status).toBe(200);
		expect(repos.repos.has(`${project}.i.fix-login`)).toBe(false);
		expect((await call("ana", "GET", `/v1/projects/${project}/checkpoints`)).body.checkpoints).toHaveLength(0);
	});
});
