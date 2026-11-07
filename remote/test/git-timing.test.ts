import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos } from "./fake-repos";

const sha = (c: string) => c.repeat(40);

async function project() {
	const repos = new FakeRepos();
	const name = `web-${crypto.randomUUID().slice(0, 8)}`;
	const { token, id } = await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS });
	const deps = {
		repos,
		fetcher: fetch,
		waitUntil: () => {},
		endpoint: async () => new Response("served", { headers: { "content-type": "application/x-git" } }),
	};
	const api = (method: string, route: string, body: unknown) =>
		handle(
			new Request(`https://newgit.dev${route}`, {
				method,
				headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
				body: body === undefined ? undefined : JSON.stringify(body),
			}),
			env,
			deps,
		);
	await api("POST", "/v1/projects", { name, trackers: ["root", "access", "web"] });
	const access = `jack.${name}.t.access`;
	repos.write(access, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(access, "web.toml", 'audience = "project-devs"\ngit = true\nmount = "web"\n');
	await api("PUT", `/v1/projects/jack/${name}/branches/main`, {
		list: { root: sha("a"), access: sha("b"), web: sha("c") },
	});
	const gitAs = (who: string) =>
		handle(
			new Request(`https://newgit.dev/jack/${name}.git/info/refs?service=git-upload-pack`, {
				headers: { authorization: `Basic ${btoa(`x:${who}`)}` },
			}),
			env,
			deps,
		);
	return { git: () => gitAs(token), gitAs, token, id, repos, api, name };
}

const header = (response: Response) => response.headers.get("server-timing") ?? "";

describe("a Git request", () => {
	it("says where its time went, and how many Artifacts calls it took", async () => {
		const { git } = await project();
		const response = await git();
		expect(response.status).toBe(200);
		expect(await response.text()).toBe("served");
		expect(response.headers.get("content-type")).toBe("application/x-git");
		const header = response.headers.get("server-timing") ?? "";
		for (const stage of ["total", "auth", "project", "definitions", "mint", "record", "endpoint"])
			expect(header).toMatch(new RegExp(`\\b${stage};dur=\\d+`));
		expect(header).toContain('mints;desc="3"');
		expect(header).toContain('trackers;desc="3"');
		expect(header).toContain('branches;desc="1"');
		expect(header).toMatch(/reads;desc="\d+"/);
	});

	it("names nothing in the header: no tracker, project, or token", async () => {
		const { git, token } = await project();
		const header = (await git()).headers.get("server-timing") ?? "";
		expect(header).not.toContain("web-");
		expect(header).not.toContain("jack");
		expect(header).not.toContain("access");
		expect(header).not.toContain(token);
	});

	it("reuses the read passwords it minted, and records each once", async () => {
		const { git, repos } = await project();
		await git();
		const minted = repos.minted.length;
		const second = await git();
		expect(second.status).toBe(200);
		expect(repos.minted.length).toBe(minted);
		expect(header(second)).not.toContain("mints;");
		expect(header(second)).not.toContain("record;dur");
	});

	it("refuses a revoked token before any password is reused", async () => {
		const { git, id, api, repos } = await project();
		await git();
		const minted = repos.minted.length;
		expect((await api("DELETE", `/v1/tokens/${id}`, undefined)).status).toBe(200);
		expect((await git()).status).toBe(401);
		expect(repos.minted.length).toBe(minted);
	});

	it("mints again when a password is near expiring", async () => {
		const { git, repos } = await project();
		const mint = repos.mint.bind(repos);
		repos.mint = async (name, scope) => ({
			...(await mint(name, scope)),
			expiresAt: new Date(Date.now() + 5 * 60_000).toISOString(),
		});
		const before = repos.minted.length;
		await git();
		const first = repos.minted.length - before;
		expect(first).toBe(3);
		await git();
		expect(repos.minted.length - before).toBe(first * 2);
	});

	it("serves a removed member nothing, whatever it holds", async () => {
		const { api, name, gitAs } = await project();
		expect((await api("POST", `/v1/projects/jack/${name}/members`, { login: "ana" })).status).toBeLessThan(300);
		const ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		expect((await gitAs(ana)).status).toBe(200);
		expect((await api("DELETE", `/v1/projects/jack/${name}/members/ana`, undefined)).status).toBeLessThan(300);
		expect((await gitAs(ana)).status).toBeGreaterThanOrEqual(400);
	});

	it("stops serving a tracker whose audience narrows, whatever it holds", async () => {
		const { api, name, gitAs, repos } = await project();
		expect((await api("POST", `/v1/projects/jack/${name}/members`, { login: "ana" })).status).toBeLessThan(300);
		const ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		const served = await gitAs(ana);
		expect(served.headers.get("server-timing")).toContain('trackers;desc="3"');
		repos.write(`jack.${name}.t.access`, "web.toml", 'audience = "user:jack"\ngit = true\nmount = "web"\n');
		const narrowed = await gitAs(ana);
		expect(narrowed.headers.get("server-timing")).toContain('trackers;desc="2"');
	});
});
