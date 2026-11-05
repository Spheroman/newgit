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
	mode: "fails" | "made-anyway" | "lookup-fails" | "in-progress" = "fails";
	/** In "in-progress": how many more look-ups say the fork is still running. */
	busy = 0;
	override async remote(name: string): Promise<string | null> {
		if (this.mode === "lookup-fails" && name.includes(".i."))
			throw Object.assign(new Error("the repository is not ready"), { code: "REPO_NOT_READY" });
		if (this.mode === "in-progress" && name.includes(".i.") && this.busy > 0) {
			this.busy -= 1;
			throw Object.assign(new Error("the fork is still in progress"), { code: "FORK_IN_PROGRESS" });
		}
		return super.remote(name);
	}
	override async fork(from: string, to: string): Promise<{ remote: string }> {
		if (this.mode === "in-progress") {
			await super.fork(from, to);
			throw Object.assign(new Error("the fork is still in progress"), { code: "FORK_IN_PROGRESS" });
		}
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

	it("says Artifacts' error when looking the fork up fails", async () => {
		repos.mode = "lookup-fails";
		const forked = await forkT3code();
		expect(forked.status).toBe(502);
		expect(forked.body.error).toContain("REPO_NOT_READY");
	});

	it("answers 202 while Artifacts is still forking, then the fork", async () => {
		repos.mode = "in-progress";
		repos.busy = 2;
		const created = await call(jack, "POST", `/v1/projects/${project}/instances`, { name: "fix", base_rev: "abc" });
		const ask = () => call(created.body.token, "POST", `/v1/projects/${project}/instances/fix/trackers/t3code/fork`);
		// Artifacts says the fork is in progress, twice: still forking.
		expect((await ask()).status).toBe(202);
		expect((await ask()).status).toBe(202);
		// Then the fork, which Artifacts makes and says is in progress, and a
		// look-up after that finds it there.
		const done = await ask();
		expect(done.status).toBe(200);
		expect(done.body.repo).toContain("t3code");
	});

	it("takes a fork that was made anyway", async () => {
		repos.mode = "made-anyway";
		const forked = await forkT3code();
		expect(forked.status).toBe(200);
	});
});
