import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, fakePassword, noEndpoint } from "./fake-repos";

let repos: FakeRepos;
let pending: Promise<unknown>[];
let project: string;
let slug: string;
const logins = new Map<string, string>();

/** A recorded login token for `who`, one per test run. */
async function tokenFor(who: string) {
	if (!logins.has(who)) logins.set(who, (await issue(env, { kind: "login", login: who, ttlSeconds: LOGIN_TTL_SECONDS })).token);
	return logins.get(who)!;
}

beforeEach(() => {
	repos = new FakeRepos();
	pending = [];
	// Durable Object storage persists across tests in a file; a fresh name
	// per test keeps them independent.
	slug = `acme-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
});

async function call(login: string | null, method: string, path: string, body?: unknown) {
	const headers: Record<string, string> = { "content-type": "application/json" };
	if (login) headers.authorization = `Bearer ${login.startsWith("ngt_") ? login : await tokenFor(login)}`;
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers,
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: (promise) => pending.push(promise), endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as any };
}

const repo = (suffix: string) => `jack.${slug}.${suffix}`;

async function setUp() {
	const created = await call("jack", "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project", "web"] });
	expect(created.status).toBe(201);
	expect((await call("jack", "POST", `/v1/projects/${project}/members`, { login: "Ana" })).status).toBe(200);
	// The project tracker's committed definitions are what audiences and kinds are read from.
	const define = (name: string, text: string) => repos.write(repo("t.access"), `.newgit/trackers/${name}.toml`, text);
	define("root", 'audience = "project-devs"\nsecret = false\n');
	define("project", 'audience = "project-devs"\nsecret = false\n');
	define("web", 'audience = "public"\nsecret = false\n');
	define("runtime-env", 'audience = "user"\n');
	define("jack-env", 'audience = "user:jack"\n');
	define("db-snapshots", 'audience = "project-devs"\n');
	return created.body;
}

const credential = (login: string, repoName: string, scope = "read") =>
	call(login, "POST", `/v1/projects/${project}/credentials`, { repo: repoName, scope });

describe("the API", () => {
	it("needs a token everywhere but login", async () => {
		expect((await call(null, "GET", "/v1/me")).status).toBe(401);
		expect((await call("jack", "GET", "/v1/me")).body).toMatchObject({ login: "jack", kind: "login" });
	});

	it("creates a project owned by its creator, with one repo and one seed per tracker", async () => {
		const created = await call("Jack", "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project"] });
		expect(created.status).toBe(201);
		expect(created.body).toMatchObject({
			owner: "jack",
			name: slug,
			key: project,
			endpoint: `https://newgit.dev/jack/${slug}.git`,
		});
		expect(created.body.seeds.map((seed: { tracker: string }) => seed.tracker)).toEqual(["root", "access", "project"]);
		const [root] = created.body.seeds;
		// The creator's one write to each code tracker's repo, for the initial push.
		expect(root).toMatchObject({
			repo: repo("t.root"),
			remote: repos.url(repo("t.root")),
			username: "x",
			password: fakePassword(repo("t.root"), "write"),
		});
		expect(root.expires_at).toBeTruthy();
		expect([...repos.repos.keys()].sort()).toEqual([repo("t.access"), repo("t.project"), repo("t.root")]);
		expect((await call("jack", "GET", `/v1/projects/${project}`)).body).toMatchObject({ owner: "jack", name: slug });
	});

	it("refuses a duplicate project, but another owner may use the same name", async () => {
		await setUp();
		expect((await call("jack", "POST", "/v1/projects", { name: slug, trackers: ["root"] })).status).toBe(409);
		const theirs = await call("ana", "POST", "/v1/projects", { name: slug, trackers: ["root"] });
		expect(theirs.status).toBe(201);
		expect(theirs.body.key).toBe(`ana/${slug}`);
	});

	it("refuses a project without a name or trackers, or with names that cannot be repos", async () => {
		for (const body of [
			{ trackers: ["root"] },
			{ name: slug },
			{ name: slug, trackers: [] },
			{ name: "a.b", trackers: ["root"] },
			{ name: slug, trackers: ["ro.ot"] },
		])
			expect((await call("jack", "POST", "/v1/projects", body)).status, JSON.stringify(body)).toBe(400);
		expect(repos.repos.size).toBe(0);
	});

	it("keeps non-members out and lets only the owner manage members", async () => {
		await setUp();
		expect((await call("eve", "GET", `/v1/projects/${project}`)).status).toBe(403);
		const info = await call("ana", "GET", `/v1/projects/${project}`);
		expect(info.body.members.map((m: { login: string }) => m.login)).toEqual(["ana", "jack"]);
		expect((await call("ana", "POST", `/v1/projects/${project}/members`, { login: "eve" })).status).toBe(403);
		expect((await call("jack", "GET", `/v1/projects/jack/nope-${slug}`)).status).toBe(404);
	});

	it("records an instance without forking anything, and hands back its endpoint and token", async () => {
		await setUp();
		const before = repos.repos.size;
		const created = await call("ana", "POST", `/v1/projects/${project}/instances`, {
			name: "fix-login",
			base_rev: "abc123",
		});
		expect(created.status).toBe(201);
		expect(created.body).toMatchObject({
			owner: "ana",
			baseRev: "abc123",
			forks: [],
			endpoint: `https://newgit.dev/jack/${slug}.git`,
		});
		expect(created.body.token.startsWith("ngt_")).toBe(true);
		expect(created.body.token_id).toBeTruthy();
		expect(repos.repos.size).toBe(before);
		expect((await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "x" })).status).toBe(409);
	});

	it("mints credentials by the rules, and says why when it will not", async () => {
		await setUp();
		await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "abc" });
		await call("ana", "POST", `/v1/projects/${project}/instances/fix-login/trackers/root/fork`);

		// A code tracker: its audience reads, nobody writes.
		expect((await credential("jack", repo("t.root"))).body.password).toBe(fakePassword(repo("t.root"), "read"));
		const noWrite = await credential("jack", repo("t.root"), "write");
		expect(noWrite.status).toBe(403);
		expect(noWrite.body.error).toContain("newgit merge");
		expect((await credential("eve", repo("t.root"))).status).toBe(403);
		// A public code tracker reads for anyone logged in.
		expect((await credential("eve", repo("t.web"))).status).toBe(200);

		// A fork: only its owner pushes. A credential helper passes a URL.
		const url = `https://acct.artifacts.example/git/newgit/${repo("i.fix-login.root")}.git`;
		expect((await credential("ana", url, "write")).status).toBe(200);
		const refused = await credential("jack", url, "write");
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("belongs to ana");
		expect((await credential("jack", url)).status).toBe(200);

		// A per-user tracker: your own repo only, created on first write.
		expect((await credential("jack", repo("t.runtime-env.jack"), "read")).status).toBe(404);
		expect((await credential("jack", repo("t.runtime-env.jack"), "write")).status).toBe(200);
		expect(repos.repos.has(repo("t.runtime-env.jack"))).toBe(true);
		expect((await credential("ana", repo("t.runtime-env.jack"))).status).toBe(403);

		// A named tracker: refused to everyone else, with the reason.
		await credential("jack", repo("t.jack-env"), "write");
		const named = await credential("ana", repo("t.jack-env"));
		expect(named.status).toBe(403);
		expect(named.body.error).toContain("user:jack");

		// Outsiders get nothing from a project-devs tracker.
		await credential("jack", repo("t.db-snapshots"), "write");
		expect((await credential("eve", repo("t.db-snapshots"))).status).toBe(403);

		// A tracker the project tracker does not define, and another project's repo.
		expect((await credential("jack", repo("t.ghost"), "write")).status).toBe(403);
		expect((await credential("jack", "other.proj.t.root")).status).toBe(400);
		expect((await credential("jack", "jack.acme")).status).toBe(400);
	});

	it("never creates a code tracker repo or a fork through the credential route", async () => {
		await setUp();
		await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "abc" });
		// web was never created as a repo here: delete it to see the refusal.
		repos.repos.delete(repo("t.web"));
		expect((await credential("jack", repo("t.web"))).status).toBe(404);
		expect((await credential("jack", repo("t.web"), "write")).status).toBe(403);
		// A fork nobody asked the fork route for does not exist, and is not made.
		expect((await credential("ana", repo("i.fix-login.root"))).status).toBe(404);
		expect((await credential("ana", repo("i.fix-login.root"), "write")).status).toBe(404);
		expect(repos.repos.has(repo("i.fix-login.root"))).toBe(false);
	});

	it("takes a definition change only from the project tracker", async () => {
		await setUp();
		await credential("jack", repo("t.jack-env"), "write");
		expect((await credential("ana", repo("t.jack-env"))).status).toBe(403);
		// A merged definition change is what opens it up.
		repos.write(repo("t.access"), ".newgit/trackers/jack-env.toml", 'audience = "project-devs"\n');
		expect((await credential("ana", repo("t.jack-env"))).status).toBe(200);
		// Turning a tracker into a code tracker takes its write away.
		repos.write(repo("t.access"), ".newgit/trackers/jack-env.toml", 'audience = "project-devs"\nsecret = false\n');
		expect((await credential("jack", repo("t.jack-env"), "write")).status).toBe(403);
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

	it("removes an instance with its checkpoints", async () => {
		await setUp();
		await call("ana", "POST", `/v1/projects/${project}/instances`, { name: "fix-login", base_rev: "abc" });
		await call("ana", "PUT", `/v1/projects/${project}/checkpoints/fix-login/ckpt_001`, { record: {} });
		expect((await call("jack", "DELETE", `/v1/projects/${project}/instances/fix-login`)).status).toBe(403);
		expect((await call("ana", "DELETE", `/v1/projects/${project}/instances/fix-login`)).status).toBe(200);
		expect((await call("ana", "GET", `/v1/projects/${project}/instances/fix-login`)).status).toBe(404);
		expect((await call("ana", "GET", `/v1/projects/${project}/checkpoints`)).body.checkpoints).toHaveLength(0);
	});

	it("creates and seeds a code tracker repo carved later, once, for its owner only", async () => {
		await setUp();
		// Definitions decide every audience, so adding a code tracker is the owner's.
		const member = await call("ana", "POST", `/v1/projects/${project}/trackers`, { name: "docs" });
		expect(member.status).toBe(403);
		const created = await call("jack", "POST", `/v1/projects/${project}/trackers`, { name: "docs" });
		expect(created.status).toBe(201);
		expect(created.body).toMatchObject({
			tracker: "docs",
			repo: repo("t.docs"),
			password: fakePassword(repo("t.docs"), "write"),
		});
		expect(repos.repos.has(repo("t.docs"))).toBe(true);
		// Not on a branch yet (a merge that did not finish): seeded again.
		const again = await call("jack", "POST", `/v1/projects/${project}/trackers`, { name: "docs" });
		expect(again.status).toBe(200);
		expect(again.body.repo).toBe(repo("t.docs"));
		// On a branch: its history moves only by merging.
		await call("jack", "PUT", `/v1/projects/${project}/branches/main`, { list: { root: "a".repeat(40) } });
		expect((await call("jack", "POST", `/v1/projects/${project}/trackers`, { name: "root" })).status).toBe(409);
		expect((await call("jack", "POST", `/v1/projects/${project}/trackers`, {})).status).toBe(400);
		expect((await call("jack", "POST", `/v1/projects/${project}/trackers`, { name: "a.b" })).status).toBe(400);
		// Never a secret tracker's repo: that is made by its first sync.
		expect(
			(await call("jack", "POST", `/v1/projects/${project}/trackers`, { name: "runtime-env" })).status,
		).toBe(400);
		expect((await call("eve", "POST", `/v1/projects/${project}/trackers`, { name: "evil" })).status).toBe(403);
	});

});
