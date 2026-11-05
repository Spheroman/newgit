import { env } from "cloudflare:workers";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A merge's finish runs Git over every tracker the merge moves before it
// lands, and the first time a large history is there (Spheroman/t3code,
// 365 MiB) that takes longer than the ten-minute merge lock. The lock was
// gone by the time the check answered, and the finish was refused (#144).
// New tests live in their own file per AGENTS.md's "Working in parallel".

let repos: FakeRepos;
let name: string;
let jack: string;

const sha = (c: string) => c.repeat(40);
const main = { root: sha("a"), access: sha("d") };

/** How long the fake container takes to check the index, in minutes. */
let checkMinutes = 0;

beforeEach(async () => {
	repos = new FakeRepos();
	checkMinutes = 0;
	name = `lock-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access"] });
	repos.write(`jack.${name}.t.access`, "root.toml", 'audience = "project-devs"\ngit = true\n');
	await call(jack, "PUT", path("/branches/main"), { list: main });
});

afterEach(() => {
	vi.useRealTimers();
});

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;

const ancestry = async (_project: string, plan: { checks: { tracker: string }[] }) =>
	plan.checks.map((check) => ({ tracker: check.tracker, forward: true, onMain: true }));

const index = async (_project: string, plan: { list: Record<string, string> }) => {
	if (checkMinutes > 0) vi.setSystemTime(Date.now() + checkMinutes * 60 * 1000);
	return { matches: true, actual: plan.list };
};

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint, ancestry, index },
	);
	return { status: response.status, body: (await response.json()) as any };
}

describe("a merge whose finish waits on a long check", () => {
	it("still holds its lock when an eleven-minute check answers", async () => {
		vi.useFakeTimers({ toFake: ["Date"] });
		const moved = { ...main, root: sha("c") };
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved })).status).toBe(200);
		checkMinutes = 11;
		const finished = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
		expect(finished.status).toBe(200);
		expect((await call(jack, "GET", path("/branches/main"))).body.list).toEqual(moved);
	});

	it("is refused when the lock had already ended before the finish", async () => {
		vi.useFakeTimers({ toFake: ["Date"] });
		const moved = { ...main, root: sha("c") };
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved })).status).toBe(200);
		vi.setSystemTime(Date.now() + 11 * 60 * 1000);
		const finished = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
		expect(finished.status).toBe(409);
		expect(finished.body.error).toContain("you hold no merge");
	});
});
