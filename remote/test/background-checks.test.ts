import { env } from "cloudflare:workers";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { BackgroundChecks, CHECK_LIMIT_MS, CHECK_RESULT_MS, CHECKING, checkKey } from "../src/endpoint";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A merge's checks run in the endpoint's container in the background: the
// first time a large history is there (Spheroman/t3code, 365 MiB) fetching
// it takes longer than any one request should wait, and every finish timed
// out (#144). The remote answers 202 `{"checking": true}` until the check
// ends, and the client asks again.
// New tests live in their own file per AGENTS.md's "Working in parallel".

let repos: FakeRepos;
let name: string;
let jack: string;
let ana: string;

const sha = (c: string) => c.repeat(40);
const main = { root: sha("a"), access: sha("d"), docs: sha("e") };

/** How many more times the fake container says it is still checking. */
let stillRunning = 0;
/** How many times a fake check was handed credentials, which starts it. */
let started = 0;
/** Minutes that pass in the fake container between one poll and the next. */
let minutesPerPoll = 0;

/** A fake background check: running for `stillRunning` more polls, then `answer`. */
function fakeCheck<A>(answer: (plan: any) => A) {
	return async (_project: string, plan: any, credentials: () => Promise<Record<string, { auth: string }>>) => {
		for (const repo of Object.values(plan.repos) as { auth: string }[])
			expect(repo.auth, "a poll carries no credentials").toBe("");
		if (minutesPerPoll > 0) vi.setSystemTime(Date.now() + minutesPerPoll * 60_000);
		if (started === 0) {
			started++;
			for (const repo of Object.values(await credentials())) expect(repo.auth).toMatch(/^Authorization: Basic /);
		}
		if (stillRunning > 0) {
			stillRunning--;
			return CHECKING;
		}
		return answer(plan);
	};
}

const ancestry = fakeCheck((plan: { checks: { tracker: string }[] }) =>
	plan.checks.map((check) => ({ tracker: check.tracker, forward: true, onMain: true })),
);
const index = fakeCheck((plan: { list: Record<string, string> }) => ({ matches: true, actual: plan.list }));

beforeEach(async () => {
	repos = new FakeRepos();
	stillRunning = 0;
	started = 0;
	minutesPerPoll = 0;
	name = `bg-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access"] });
	await call(jack, "POST", path("/members"), { login: "ana" });
	repos.write(`jack.${name}.t.access`, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(`jack.${name}.t.access`, "docs.toml", 'audience = "project-devs"\ngit = true\n');
	await repos.create(`jack.${name}.t.docs`);
	await call(jack, "PUT", path("/branches/main"), { list: main });
});

afterEach(() => {
	vi.useRealTimers();
});

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;

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
	return {
		status: response.status,
		retryAfter: response.headers.get("retry-after"),
		body: (await response.json()) as any,
	};
}

describe("a merge's finish while the index check runs", () => {
	it("answers 202 on every poll, holds the lock throughout, and lands on a later poll", async () => {
		vi.useFakeTimers({ toFake: ["Date"] });
		const moved = { ...main, root: sha("c") };
		expect((await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved })).status).toBe(200);
		const readsBefore = repos.minted.filter((token) => token.scope === "read").length;

		// Eight polls, four minutes apart: 32 minutes, past the ten-minute
		// merge lock and past any one extension of it.
		stillRunning = 8;
		minutesPerPoll = 4;
		for (let poll = 0; poll < 8; poll++) {
			const answer = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
			expect(answer.status, `poll ${poll}: ${JSON.stringify(answer.body)}`).toBe(202);
			expect(answer.body).toEqual({ checking: true });
			expect(answer.retryAfter).toBe("10");
			// Still jack's merge: nobody else may begin one.
			const other = await call(ana, "POST", path("/branches/main/merge"), {
				base: main,
				list: { ...main, root: sha("b") },
			});
			expect(other.status).toBe(409);
			expect(other.body.error).toContain("jack is merging");
		}
		expect((await call(jack, "GET", path("/branches/main"))).body.list, "nothing moved yet").toEqual(main);

		const landed = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
		expect(landed.status).toBe(200);
		expect((await call(jack, "GET", path("/branches/main"))).body.list).toEqual(moved);
		// Read passwords were minted once, for the check that started, not
		// for every poll.
		expect(started).toBe(1);
		expect(repos.minted.filter((token) => token.scope === "read").length - readsBefore).toBe(Object.keys(moved).length);
	});
});

describe("a merge's begin while the ancestry check runs", () => {
	it("answers 202 without taking the lock, then begins on a later poll", async () => {
		const moved = { ...main, root: sha("c"), docs: sha("f") };
		const writesBefore = repos.minted.filter((token) => token.scope === "write").length;
		stillRunning = 3;
		for (let poll = 0; poll < 3; poll++) {
			const answer = await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved });
			expect(answer.status, JSON.stringify(answer.body)).toBe(202);
			expect(answer.body).toEqual({ checking: true });
			expect(answer.retryAfter).toBe("10");
			const writes = repos.minted.filter((token) => token.scope === "write").length;
			expect(writes, "no write password while it checks").toBe(writesBefore);
		}
		const begun = await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved });
		expect(begun.status, JSON.stringify(begun.body)).toBe(200);
		expect(begun.body.credentials.map((credential: { tracker: string }) => credential.tracker)).toEqual(["root"]);
		expect(started).toBe(1);
	});
});

describe("the endpoint's background checks", () => {
	it("says running until the check ends, then hands its answer over once", async () => {
		const checks = new BackgroundChecks();
		let finish!: (json: string) => void;
		void checks.start("k", () => new Promise((resolve) => (finish = (json) => resolve({ ok: true, json }))));
		expect(checks.collect("k")).toBe("running");
		expect(checks.running()).toBe(1);
		// Asking again with the same key does not start a second one.
		let second = false;
		void checks.start("k", async () => {
			second = true;
			return { ok: true, json: "[]" };
		});
		expect(second).toBe(false);
		finish('{"matches":true}');
		await Promise.resolve();
		await Promise.resolve();
		expect(checks.running()).toBe(0);
		expect(checks.collect("k")).toEqual({ ok: true, json: '{"matches":true}' });
		expect(checks.collect("k"), "collected once, then forgotten").toBeUndefined();
	});

	it("keeps a failure's reason, and forgets an answer nobody collects", async () => {
		let now = 0;
		const checks = new BackgroundChecks(() => now);
		await checks.start("bad", async () => ({ ok: false, why: "fatal: expected 'packfile'" }));
		expect(checks.collect("bad")).toEqual({ ok: false, why: "fatal: expected 'packfile'" });
		await checks.start("late", async () => ({ ok: true, json: "[]" }));
		now += CHECK_RESULT_MS + 1;
		expect(checks.collect("late")).toBeUndefined();
	});

	it("counts a check running past the limit as failed", async () => {
		let now = 0;
		const checks = new BackgroundChecks(() => now);
		let finish!: () => void;
		const run = checks.start(
			"slow",
			() => new Promise((resolve) => (finish = () => resolve({ ok: true, json: "[]" }))),
		);
		now += CHECK_LIMIT_MS + 1;
		const outcome = checks.collect("slow");
		expect(outcome).toMatchObject({ ok: false });
		expect((outcome as { why: string }).why).toContain("45 minutes");
		// The old run ending later does not answer for a new one.
		void checks.start("slow", () => new Promise(() => {}));
		finish();
		await run;
		expect(checks.collect("slow")).toBe("running");
	});

	it("keys a check by its plan without credentials", async () => {
		const plan = (auth: string) => ({
			dir: "/work",
			repos: { root: { url: "https://a/root.git", auth } },
			list: { root: sha("a") },
		});
		expect(await checkKey("index", plan("Authorization: Basic one"))).toBe(await checkKey("index", plan("")));
		expect(await checkKey("index", plan(""))).not.toBe(await checkKey("ancestry", plan("")));
		expect(await checkKey("index", plan(""))).not.toBe(
			await checkKey("index", { ...plan(""), list: { root: sha("b") } }),
		);
	});
});
