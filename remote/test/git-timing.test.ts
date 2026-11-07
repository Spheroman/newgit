import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos } from "./fake-repos";

const sha = (c: string) => c.repeat(40);

async function project() {
	const repos = new FakeRepos();
	const name = `web-${crypto.randomUUID().slice(0, 8)}`;
	const { token } = await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS });
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
				body: JSON.stringify(body),
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
	const git = () =>
		handle(
			new Request(`https://newgit.dev/jack/${name}.git/info/refs?service=git-upload-pack`, {
				headers: { authorization: `Basic ${btoa(`x:${token}`)}` },
			}),
			env,
			deps,
		);
	return { git, token };
}

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
});
