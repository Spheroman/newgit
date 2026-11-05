import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

// An instance's fork of Spheroman/t3code (365 MiB) failed on newgit.dev with
// only "internal error" to show for it (#144). A fork that fails now says
// Artifacts' own error, and one that finished despite the error is taken.
// New tests live in their own file per AGENTS.md's "Working in parallel".

/** Repos whose fork fails, either outright or after making the fork anyway. */
class FailingForks extends FakeRepos {
	mode: "fails" | "made-anyway" = "fails";
	override async fork(from: string, to: string): Promise<{ remote: string }> {
		if (this.mode === "made-anyway") await super.fork(from, to);
		throw Object.assign(new Error("the fork did not finish"), { code: "INTERNAL_ERROR" });
	}
}

let repos: FailingForks;
let slug: string;
let project: string;
let jack: string;

beforeEach(async () => {
	repos = new FailingForks();
	slug = `fork-${crypto.randomUUID().slice(0, 8)}`;
	project = `jack/${slug}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect((await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "t3code"] })).status).toBe(201);
	repos.write(`jack.${slug}.t.access`, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(`jack.${slug}.t.access`, "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\n');
	repos.write(`jack.${slug}.t.t3code`, "README.md", "t3\n");
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

async function forkT3code() {
	const created = await call(jack, "POST", `/v1/projects/${project}/instances`, { name: "fix", base_rev: "abc" });
	expect(created.status).toBe(201);
	return call(created.body.token, "POST", `/v1/projects/${project}/instances/fix/trackers/t3code/fork`);
}

describe("an instance's fork that Artifacts fails", () => {
	it("says Artifacts' error", async () => {
		const forked = await forkT3code();
		expect(forked.status).toBe(502);
		expect(forked.body.error).toContain("INTERNAL_ERROR");
		expect(forked.body.error).toContain("t3code");
	});

	it("takes a fork that was made anyway", async () => {
		repos.mode = "made-anyway";
		const forked = await forkT3code();
		expect(forked.status).toBe(200);
	});
});
