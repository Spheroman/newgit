import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { CHECKING, type CopyPlan } from "../src/endpoint";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

// Artifacts' own fork of Spheroman/t3code (365 MiB) never finished: it said
// FORK_IN_PROGRESS for hours (#144). An instance's fork is now an empty repo
// the endpoint's container copies the tracker's branches into, in the
// background; the route answers 202 until the copy is done.
// New tests live in their own file per AGENTS.md's "Working in parallel".

/** Repos where a fork can be left stuck by Artifacts' old fork. */
class StuckRepos extends FakeRepos {
	/** Repos whose look-up says FORK_IN_PROGRESS until they are deleted. */
	readonly stuck = new Set<string>();
	refuseDelete = false;
	readonly deleted: string[] = [];
	override async remote(name: string) {
		if (this.stuck.has(name))
			throw Object.assign(new Error("a fork is already in progress"), { code: "FORK_IN_PROGRESS" });
		return super.remote(name);
	}
	override async delete(name: string) {
		if (this.refuseDelete)
			throw Object.assign(new Error("the repository is busy"), { code: "FORK_IN_PROGRESS" });
		this.deleted.push(name);
		this.stuck.delete(name);
		return super.delete(name);
	}
}

let repos: StuckRepos;
let slug: string;
let project: string;
let jack: string;

/** The fake container's copy: running for `polls` more asks, then `outcome`. */
let polls: number;
let outcome: "copied" | "fails";
/** How many copies were started: handed their passwords. */
let started: number;
let running: boolean;

const copy = async (
	_project: string,
	plan: CopyPlan,
	credentials: () => Promise<Pick<CopyPlan, "from" | "to">>,
) => {
	expect(plan.from.auth, "an ask carries no passwords").toBe("");
	if (!running) {
		const minted = await credentials();
		expect(minted.from.auth).toMatch(/^Authorization: Basic /);
		expect(minted.to.auth).toMatch(/^Authorization: Basic /);
		started++;
		running = true;
	}
	if (polls > 0) {
		polls--;
		return CHECKING;
	}
	running = false;
	if (outcome === "fails")
		throw new Error(
			"its Git endpoint said: newgit endpoint-copy failed: error: RPC failed; HTTP 500 curl 22 The requested URL returned error: 500",
		);
	const from = repos.repos.get(repos.nameOf(plan.from.url))!;
	const to = repos.repos.get(repos.nameOf(plan.to.url))!;
	for (const [path, text] of from) to.set(path, text);
	return { branches: { main: "a".repeat(40) } };
};

const fork = () => `jack.${slug}.i.fix.t3code`;

beforeEach(async () => {
	repos = new StuckRepos();
	polls = 0;
	outcome = "copied";
	started = 0;
	running = false;
	slug = `copy-${crypto.randomUUID().slice(0, 8)}`;
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
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint, copy },
	);
	return { status: response.status, headers: response.headers, body: (await response.json()) as any };
}

async function instance() {
	const created = await call(jack, "POST", `/v1/projects/${project}/instances`, { name: "fix", base_rev: "abc" });
	expect(created.status).toBe(201);
	return created.body.token as string;
}

const ask = (token: string) => call(token, "POST", `/v1/projects/${project}/instances/fix/trackers/t3code/fork`);

describe("an instance's fork, copied by the remote", () => {
	it("answers 202 while the copy runs, then the fork", async () => {
		const token = await instance();
		const before = repos.minted.length;
		polls = 2;
		const first = await ask(token);
		expect(first.status).toBe(202);
		expect(first.body).toEqual({ forking: true });
		expect(first.headers.get("retry-after")).toBe("5");
		// Made empty, and not handed out while it fills.
		expect(repos.repos.get(fork())?.size).toBe(0);
		const password = await call(token, "POST", `/v1/projects/${project}/credentials`, { repo: fork(), scope: "write" });
		expect(password.status).toBe(404);
		expect((await call(jack, "GET", `/v1/projects/${project}/instances/fix`)).body.forks).toEqual([]);

		expect((await ask(token)).status).toBe(202);
		const done = await ask(token);
		expect(done.status).toBe(200);
		expect(done.body).toEqual({ instance: "fix", tracker: "t3code", repo: fork(), remote: repos.url(fork()) });
		expect(repos.repos.get(fork())?.get("README.md")).toBe("t3\n");
		expect(started).toBe(1);
		// The copy's passwords: read on the tracker, write on the fork.
		expect(repos.minted.slice(before).map(({ name, scope }) => [name, scope])).toEqual([
			[`jack.${slug}.t.t3code`, "read"],
			[fork(), "write"],
		]);
		expect((await call(jack, "GET", `/v1/projects/${project}/instances/fix`)).body.forks).toEqual(["t3code"]);
	});

	it("answers a copy that failed with Git's error, and copies again when asked again", async () => {
		const token = await instance();
		outcome = "fails";
		const failed = await ask(token);
		expect(failed.status).toBe(502);
		expect(failed.body.error).toContain("t3code");
		expect(failed.body.error).toContain("RPC failed; HTTP 500");
		// Its repo is left there, part way, and not handed out.
		expect(repos.repos.has(fork())).toBe(true);
		expect((await call(jack, "GET", `/v1/projects/${project}/instances/fix`)).body.forks).toEqual([]);

		// Asked again, the copy carries on into the same repo and finishes.
		outcome = "copied";
		repos.write(fork(), "half.txt", "from the copy that died\n");
		const done = await ask(token);
		expect(done.status).toBe(200);
		expect(started).toBe(2);
		expect(repos.repos.get(fork())?.get("README.md")).toBe("t3\n");
	});

	it("deletes a fork left stuck in FORK_IN_PROGRESS and makes it again", async () => {
		const token = await instance();
		await repos.create(fork());
		repos.stuck.add(fork());
		const done = await ask(token);
		expect(done.status).toBe(200);
		expect(repos.deleted).toEqual([fork()]);
		expect(repos.repos.get(fork())?.get("README.md")).toBe("t3\n");
	});

	it("answers 502 with Artifacts' error when it refuses to delete a stuck fork", async () => {
		const token = await instance();
		await repos.create(fork());
		repos.stuck.add(fork());
		repos.refuseDelete = true;
		const refused = await ask(token);
		expect(refused.status).toBe(502);
		expect(refused.body.error).toContain("FORK_IN_PROGRESS");
		expect(refused.body.error).toContain("the repository is busy");
		expect(started).toBe(0);
	});

	it("returns a complete fork as it is, without copying again", async () => {
		const token = await instance();
		expect((await ask(token)).status).toBe(200);
		repos.write(fork(), "work.txt", "the instance's work\n");
		repos.write(`jack.${slug}.t.t3code`, "README.md", "moved on\n");
		const again = await ask(token);
		expect(again.status).toBe(200);
		expect(again.body.remote).toBe(repos.url(fork()));
		expect(started).toBe(1);
		expect(repos.repos.get(fork())?.get("work.txt")).toBe("the instance's work\n");
		expect(repos.repos.get(fork())?.get("README.md")).toBe("t3\n");
	});

	it("removing the instance deletes a fork whose copy never finished", async () => {
		const token = await instance();
		polls = 5;
		expect((await ask(token)).status).toBe(202);
		expect((await call(jack, "DELETE", `/v1/projects/${project}/instances/fix`)).status).toBe(200);
		expect(repos.repos.has(fork())).toBe(false);
	});
});
