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
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "project", "web"] })).status).toBe(201);
	expect((await call(jack, "POST", `/v1/projects/${project}/members`, { login: "ana" })).status).toBe(200);
	const define = (name: string, text: string) => repos.write(repo("t.access"), `.newgit/trackers/${name}.toml`, text);
	define("root", 'audience = "project-devs"\nsecret = false\n');
	define("web", 'audience = "project-devs"\nsecret = false\n');
	define("runtime-env", 'audience = "user"\n');
	repos.write(repo("t.root"), "README.md", "hello\n");
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

const spawn = async (token: string, name: string) => {
	const created = await call(token, "POST", `/v1/projects/${project}/instances`, { name, base_rev: "abc" });
	expect(created.status).toBe(201);
	return created.body as { token: string };
};
const fork = (token: string, instance: string, tracker: string) =>
	call(token, "POST", `/v1/projects/${project}/instances/${instance}/trackers/${tracker}/fork`);

describe("forking a code tracker", () => {
	it("creates the instance's fork of that tracker, with its content", async () => {
		await spawn(ana, "fix-login");
		const forked = await fork(ana, "fix-login", "root");
		expect(forked.status).toBe(200);
		expect(forked.body).toMatchObject({
			instance: "fix-login",
			tracker: "root",
			repo: repo("i.fix-login.root"),
			remote: repos.url(repo("i.fix-login.root")),
		});
		expect(repos.repos.get(repo("i.fix-login.root"))?.get("README.md")).toBe("hello\n");
		// Only the tracker asked for.
		expect(repos.repos.has(repo("i.fix-login.web"))).toBe(false);
	});

	it("is idempotent: asking again leaves the fork alone and records it once", async () => {
		await spawn(ana, "fix-login");
		await fork(ana, "fix-login", "root");
		repos.write(repo("i.fix-login.root"), "work.txt", "in progress\n");
		const again = await fork(ana, "fix-login", "root");
		expect(again.status).toBe(200);
		expect(again.body.remote).toBe(repos.url(repo("i.fix-login.root")));
		expect(repos.repos.get(repo("i.fix-login.root"))?.get("work.txt")).toBe("in progress\n");
		const instance = await call(ana, "GET", `/v1/projects/${project}/instances/fix-login`);
		expect(instance.body.forks).toEqual(["root"]);
	});

	it("lists every tracker the instance forked, in the instance's record", async () => {
		await spawn(ana, "fix-login");
		expect((await call(ana, "GET", `/v1/projects/${project}/instances/fix-login`)).body.forks).toEqual([]);
		await fork(ana, "fix-login", "web");
		await fork(ana, "fix-login", "root");
		expect((await call(ana, "GET", `/v1/projects/${project}/instances/fix-login`)).body.forks).toEqual(["root", "web"]);
		const listed = await call(ana, "GET", `/v1/projects/${project}/instances`);
		expect(listed.body.instances[0].forks).toEqual(["root", "web"]);
	});

	it("is refused for a secret tracker, and for one the project does not define", async () => {
		await spawn(ana, "fix-login");
		const secret = await fork(ana, "fix-login", "runtime-env");
		expect(secret.status).toBe(400);
		expect(secret.body.error).toContain("secret tracker");
		expect((await fork(ana, "fix-login", "ghost")).status).toBe(404);
		expect(repos.repos.has(repo("i.fix-login.runtime-env"))).toBe(false);
		expect((await call(ana, "GET", `/v1/projects/${project}/instances/fix-login`)).body.forks).toEqual([]);
	});

	it("is for the instance's owner, not another member", async () => {
		await spawn(ana, "fix-login");
		const refused = await fork(jack, "fix-login", "root");
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("only ana");
		expect(repos.repos.has(repo("i.fix-login.root"))).toBe(false);
	});

	it("is for the instance's own token, not another instance's", async () => {
		const one = await spawn(jack, "attempt-1");
		await spawn(jack, "attempt-2");
		expect((await fork(one.token, "attempt-1", "root")).status).toBe(200);
		const refused = await fork(one.token, "attempt-2", "root");
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("`attempt-1` only");
		expect(repos.repos.has(repo("i.attempt-2.root"))).toBe(false);
	});

	it("says 404 for an instance that does not exist", async () => {
		expect((await fork(jack, "ghost", "root")).status).toBe(404);
	});

	it("needs a project member", async () => {
		await spawn(ana, "fix-login");
		expect((await fork(await login("eve"), "fix-login", "root")).status).toBe(403);
	});

	it("makes a fork its owner may push to, and other members may only read", async () => {
		await spawn(ana, "fix-login");
		await fork(ana, "fix-login", "root");
		const credential = (token: string, scope: string) =>
			call(token, "POST", `/v1/projects/${project}/credentials`, { repo: repo("i.fix-login.root"), scope });
		expect((await credential(ana, "write")).status).toBe(200);
		expect((await credential(jack, "read")).status).toBe(200);
		expect((await credential(jack, "write")).status).toBe(403);
	});
});

describe("removing an instance", () => {
	it("deletes its forks and no one else's, and revokes their passwords", async () => {
		const one = await spawn(ana, "attempt-1");
		await spawn(ana, "attempt-2");
		await fork(one.token, "attempt-1", "root");
		await fork(one.token, "attempt-1", "web");
		await fork(ana, "attempt-2", "root");
		const read = (name: string) =>
			call(one.token, "POST", `/v1/projects/${project}/credentials`, { repo: name, scope: "write" });
		expect((await read(repo("i.attempt-1.root"))).status).toBe(200);
		const minted = repos.minted.at(-1)!.id;

		expect((await call(ana, "DELETE", `/v1/projects/${project}/instances/attempt-1`)).status).toBe(200);
		expect(repos.repos.has(repo("i.attempt-1.root"))).toBe(false);
		expect(repos.repos.has(repo("i.attempt-1.web"))).toBe(false);
		expect(repos.repos.has(repo("i.attempt-2.root"))).toBe(true);
		// The code trackers themselves are untouched.
		expect(repos.repos.has(repo("t.root"))).toBe(true);
		expect(repos.revokedTokens.has(minted)).toBe(true);
		expect((await call(one.token, "GET", "/v1/me")).status).toBe(401);
	});
});
