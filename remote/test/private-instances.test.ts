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

const spawn = async (token: string, name: string, isPrivate: boolean) => {
	const created = await call(token, "POST", `/v1/projects/${project}/instances`, {
		name,
		base_rev: "abc",
		private: isPrivate,
	});
	expect(created.status).toBe(201);
	return created.body as { token: string; private: boolean };
};

describe("a private instance", () => {
	beforeEach(async () => {
		expect((await spawn(ana, "fix-cve", true)).private).toBe(true);
		expect(
			(await call(ana, "POST", `/v1/projects/${project}/instances/fix-cve/trackers/root/fork`)).status,
		).toBe(200);
		expect(
			(await call(ana, "PUT", `/v1/projects/${project}/checkpoints/fix-cve/ckpt_001`, { record: { note: "the fix" } }))
				.status,
		).toBe(201);
	});

	it("is there for its owner as for any instance", async () => {
		expect((await call(ana, "GET", `/v1/projects/${project}/instances/fix-cve`)).status).toBe(200);
		const listed = await call(ana, "GET", `/v1/projects/${project}/instances`);
		expect(listed.body.instances.map((i: any) => i.name)).toEqual(["fix-cve"]);
		expect((await call(ana, "GET", `/v1/projects/${project}/checkpoints`)).body.checkpoints).toHaveLength(1);
		const read = await call(ana, "POST", `/v1/projects/${project}/credentials`, {
			repo: repo("i.fix-cve.root"),
			scope: "read",
		});
		expect(read.status).toBe(200);
	});

	it("is not there for anyone else, even the project's owner", async () => {
		expect((await call(jack, "GET", `/v1/projects/${project}/instances/fix-cve`)).status).toBe(404);
		expect((await call(jack, "GET", `/v1/projects/${project}/instances`)).body.instances).toEqual([]);
		expect((await call(jack, "GET", `/v1/projects/${project}/checkpoints`)).body.checkpoints).toEqual([]);
		expect(
			(await call(jack, "GET", `/v1/projects/${project}/checkpoints?instance=fix-cve`)).body.checkpoints,
		).toEqual([]);
		expect((await call(jack, "GET", `/v1/projects/${project}/checkpoints/fix-cve/ckpt_001`)).status).toBe(404);

		const read = await call(jack, "POST", `/v1/projects/${project}/credentials`, {
			repo: repo("i.fix-cve.root"),
			scope: "read",
		});
		expect(read.status).toBe(403);
		// The same answer as an instance that does not exist.
		expect(read.body.error).toContain("no instance `fix-cve`");
		expect((await call(jack, "DELETE", `/v1/projects/${project}/instances/fix-cve`)).status).toBe(404);
	});

	it("leaves other instances as they were", async () => {
		await spawn(jack, "docs", false);
		expect((await call(ana, "GET", `/v1/projects/${project}/instances/docs`)).body.private).toBe(false);
		const listed = await call(jack, "GET", `/v1/projects/${project}/instances`);
		expect(listed.body.instances.map((i: any) => i.name)).toEqual(["docs"]);
	});
});
