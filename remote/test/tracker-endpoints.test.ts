// A Git endpoint for every tracker, by path (docs/design/anchoring.md,
// *Step 8*): `/<owner>/<name>/<path>.git` is a view rooted at the tracker at
// that path, cut down to it and what is inside it. New tests live in their
// own file per AGENTS.md's "Working in parallel".

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
		(await api(jack.token, "POST", "/v1/projects", { name, trackers: ["root", "access", "t3code", "payments"] })).status,
	).toBe(201);
	const defs = `jack.${name}.t.access`;
	repos.write(defs, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(defs, "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\n');
	// A tracker in Git only Jack may read, and a tracker outside Git: neither is Ana's.
	repos.write(defs, "payments.toml", 'audience = "user:jack"\ngit = true\n');
	repos.write(defs, "env.toml", 'audience = "project-devs"\n');
	expect(
		(
			await api(jack.token, "PUT", `/v1/projects/jack/${name}/branches/main`, {
				list: { root: sha("a"), t3code: sha("b"), payments: sha("c"), env: sha("d"), access: sha("e") },
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

/** What Git sends to `/jack/<name><at>.git`: Basic auth, the token as the password. */
async function git(token: string, at: string, path: string) {
	const headers = { authorization: `Basic ${btoa(`x:${token}`)}` };
	return handle(new Request(`https://newgit.dev/jack/${name}${at}.git${path}`, { headers }), env, deps());
}

describe("a tracker's own Git endpoint", () => {
	it("serves a view rooted at the tracker its path names", async () => {
		const response = await git(jack.token, "/t3code", "/info/refs?service=git-upload-pack");
		expect(response.status).toBe(200);
		const plan = served.at(-1)!.plan;
		expect(plan.root).toBe("t3code");
		expect(plan.branches.main).toEqual({ t3code: sha("b") });
	});

	it("is the root's view without a path, as before", async () => {
		await git(jack.token, "", "/info/refs?service=git-upload-pack");
		const plan = served.at(-1)!.plan;
		expect(plan.root).toBe("root");
		expect(plan.branches.main.root).toBe(sha("a"));
	});

	it("is not served to someone who may not read that tracker, nor for one that is not there", async () => {
		expect((await git(ana, "/payments", "/info/refs?service=git-upload-pack")).status).toBe(403);
		expect((await git(jack.token, "/nope", "/info/refs?service=git-upload-pack")).status).toBe(404);
	});
});
