import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { CHECKING, type PullAnswer, type PullPlan, pullKey } from "../src/endpoint";
import { type Deps, handle, issue } from "../src/index";
import { PULL_ATTEMPTS, PULL_LIMIT_MS, advancePulls, githubRepo, verifySignature } from "../src/webhook";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A pull request merged on a mirror comes back through GitHub's webhook: a
// signed push event, a pull in the endpoint's container, and `main` moved the
// way a merge moves it. The container is faked here; `newgit endpoint-pull`
// has its tests in crates/newgit-core/tests/endpoint_pull.rs.
// New tests live in their own file per AGENTS.md's "Working in parallel".

const SECRET = "shh, webhook";
const secretEnv = new Proxy(env, {
	get: (target, key) => (key === "GITHUB_WEBHOOK_SECRET" ? SECRET : Reflect.get(target, key)),
}) as Env;

const sha = (c: string) => c.repeat(40);
const main = { root: sha("a"), access: sha("d"), t3code: sha("e") };
const THEIRS = sha("b");

let repos: FakeRepos;
let name: string;
let jack: string;
let waited: Promise<unknown>[];
/** What the fake container does next. */
let answers: (PullAnswer | typeof CHECKING | Error)[];
let plans: PullPlan[];
let started: PullPlan["repos"][];

const pull: Deps["pull"] = async (_project, plan, credentials) => {
	plans.push(plan);
	for (const repo of Object.values(plan.repos)) expect(repo.auth, "an ask carries no passwords").toBe("");
	const answer = answers.shift() ?? { outcome: "nothing" };
	if (started.length < plans.length) started.push(await credentials());
	if (answer instanceof Error) throw answer;
	return answer as PullAnswer;
};

const deps = (): Deps => ({
	repos,
	fetcher: fetch,
	waitUntil: (promise) => waited.push(promise),
	endpoint: noEndpoint,
	pull,
	ancestry: async (_project, plan) =>
		plan.checks.map((check) => ({ tracker: check.tracker, forward: true, onMain: true })),
	index: async (_project, plan) => ({ matches: true, actual: plan.list }),
});

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		secretEnv,
		deps(),
	);
	return { status: response.status, body: (await response.json()) as any };
}

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;
const stub = () => env.PROJECTS.getByName(`jack/${name}`);
const index = async () => (await call(jack, "GET", path("/branches/main"))).body.list;

async function sign(secret: string, body: string) {
	const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
		"sign",
	]);
	const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(body)));
	return `sha256=${[...mac].map((byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

const push = (overrides: Record<string, unknown> = {}) => ({
	ref: "refs/heads/main",
	after: THEIRS,
	repository: { full_name: "Spheroman/newgit", private: false },
	pusher: { name: "jack" },
	...overrides,
});

interface Delivery {
	event?: string;
	payload?: unknown;
	signature?: string | null;
	query?: string;
	useEnv?: Env;
}

async function deliver({ event = "push", payload = push(), signature, query, useEnv = secretEnv }: Delivery = {}) {
	const body = JSON.stringify(payload);
	const headers = new Headers({ "x-github-event": event });
	const signed = signature === undefined ? await sign(SECRET, body) : signature;
	if (signed !== null) headers.set("x-hub-signature-256", signed);
	const response = await handle(
		new Request(`https://newgit.dev/v1/webhooks/github?${query ?? `project=jack/${name}&target=github`}`, {
			method: "POST",
			headers,
			body,
		}),
		useEnv,
		deps(),
	);
	await Promise.all(waited);
	waited = [];
	return { status: response.status, body: (await response.json()) as any };
}

const advance = () => advancePulls(secretEnv, deps(), { owner: "jack", name });

beforeEach(async () => {
	repos = new FakeRepos();
	waited = [];
	answers = [];
	plans = [];
	started = [];
	name = `hook-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "t3code"] });
	repos.write(`jack.${name}.t.access`, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(
		`jack.${name}.t.access`,
		"t3code.toml",
		'audience = "public"\ngit = true\nmount = "apps/t3code"\npaths = ["."]\n',
	);
	repos.write(
		`jack.${name}.t.root`,
		".newgit/exports/github.toml",
		'url = "Spheroman/newgit"\ntrackers = ["root", "access"]\n',
	);
	repos.write(`jack.${name}.t.root`, ".newgit/exports/t3code.toml", 'url = "Spheroman/t3code"\nroot = "t3code"\n');
	await call(jack, "PUT", path("/branches/main"), { list: main });
});

describe("what gets in", () => {
	it("answers 503 without the secret configured", async () => {
		const unset = new Proxy(env, {
			get: (target, key) => (key === "GITHUB_WEBHOOK_SECRET" ? undefined : Reflect.get(target, key)),
		}) as Env;
		expect((await deliver({ useEnv: unset })).status).toBe(503);
	});

	it("refuses a missing signature", async () => {
		expect((await deliver({ signature: null })).status).toBe(401);
		expect(await stub().pulls()).toEqual([]);
	});

	it("refuses a signature made with another secret, or of another body", async () => {
		expect((await deliver({ signature: await sign("guess", JSON.stringify(push())) })).status).toBe(401);
		expect((await deliver({ signature: await sign(SECRET, "something else") })).status).toBe(401);
		expect((await deliver({ signature: "sha256=zz" })).status).toBe(401);
		expect(await stub().pulls()).toEqual([]);
	});

	it("answers ping", async () => {
		const answer = await deliver({ event: "ping", payload: { zen: "Keep it logically awesome." } });
		expect(answer.status).toBe(200);
	});

	it("ignores other events", async () => {
		const answer = await deliver({ event: "issues", payload: { action: "opened" } });
		expect(answer.status).toBe(200);
		expect(await stub().pulls()).toEqual([]);
	});

	it("ignores a branch that is not the target's", async () => {
		const answer = await deliver({ payload: push({ ref: "refs/heads/feature" }) });
		expect(answer.status).toBe(200);
		expect(await stub().pulls()).toEqual([]);
	});

	it("ignores a deleted branch", async () => {
		const answer = await deliver({ payload: push({ deleted: true, after: "0".repeat(40) }) });
		expect(answer.status).toBe(200);
		expect(await stub().pulls()).toEqual([]);
	});

	it("ignores a rooted target: its commits are its tracker's", async () => {
		const answer = await deliver({
			payload: push({ repository: { full_name: "Spheroman/t3code" } }),
			query: `project=jack/${name}&target=t3code`,
		});
		expect(answer.status).toBe(200);
		expect(answer.body.ignored).toContain("rooted");
		expect(await stub().pulls()).toEqual([]);
	});

	it("does not let one repo's event move a project that does not mirror there", async () => {
		const answer = await deliver({ payload: push({ repository: { full_name: "Attacker/newgit" } }) });
		expect(answer.status).toBe(403);
		expect(await stub().pulls()).toEqual([]);
	});

	it("says what is wrong with the URL", async () => {
		expect((await deliver({ query: "target=github" })).status).toBe(400);
		expect((await deliver({ query: `project=jack/${name}` })).status).toBe(400);
		expect((await deliver({ query: `project=jack/${name}&target=nope` })).status).toBe(404);
		expect((await deliver({ query: "project=jack/nobody&target=github" })).status).toBe(404);
	});

	it("refuses a private mirror, which the endpoint cannot read", async () => {
		const answer = await deliver({ payload: push({ repository: { full_name: "Spheroman/newgit", private: true } }) });
		expect(answer.status).toBe(422);
	});

	it("refuses a pusher who cannot merge here", async () => {
		const answer = await deliver({ payload: push({ pusher: { name: "stranger" } }) });
		expect(answer.status).toBe(403);
		expect(await stub().pulls()).toEqual([]);
	});
});

describe("a good event", () => {
	it("is answered at once and starts the pull with what the pusher may write", async () => {
		const moved = { ...main, root: sha("c") };
		answers = [{ outcome: "landed", list: moved, commits: 2 }];
		const answer = await deliver();
		expect(answer.status).toBe(202);
		expect(plans[0].list).toEqual(main);
		expect(plans[0].target).toBe("github");
		expect(plans[0].mirror).toEqual({
			url: "https://github.com/Spheroman/newgit.git",
			branch: "main",
			commit: THEIRS,
		});
		expect([...plans[0].writable].sort()).toEqual(["access", "root"]);
		expect(await index()).toEqual(moved);
		expect(await stub().pulls()).toEqual([]);
		// t3code is withheld: a read password, and it is cancelled with the rest.
		const minted = repos.minted.filter((token) => token.name.endsWith(".t.t3code") && token.id);
		expect(minted.at(-1)?.scope).toBe("read");
		expect(repos.minted.find((token) => token.name.endsWith(".t.root") && token.scope === "write")).toBeTruthy();
		for (const token of repos.minted.slice(-3)) expect(repos.revokedTokens.has(token.id)).toBe(true);
		// The lock is gone: a merge can start.
		const again = await call(jack, "POST", path("/branches/main/merge"), {
			base: moved,
			list: { ...moved, root: sha("f") },
		});
		expect(again.status).toBe(200);
	});

	it("waits for the container, keeping the lock and starting it once", async () => {
		answers = [CHECKING, CHECKING, { outcome: "landed", list: { ...main, root: sha("c") }, commits: 1 }];
		await deliver();
		expect(await advance()).toBe(true);
		expect(await index()).toEqual(main);
		const moved = { ...main, root: sha("f") };
		const during = await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved });
		expect(during.status).toBe(409);
		expect(during.body.error).toContain("the GitHub webhook");
		expect(await advance()).toBe(false);
		expect((await index()).root).toBe(sha("c"));
		expect(new Set(plans.map(pullKey)).size).toBe(1);
	});

	it("does nothing on a redelivery, or on an event for our own export", async () => {
		answers = [{ outcome: "nothing" }, { outcome: "nothing" }];
		expect((await deliver()).status).toBe(202);
		expect((await deliver()).status).toBe(202);
		expect(await index()).toEqual(main);
		expect(await stub().pulls()).toEqual([]);
		const free = await call(jack, "POST", path("/branches/main/merge"), {
			base: main,
			list: { ...main, root: sha("f") },
		});
		expect(free.status).toBe(200);
	});

	it("queues a redelivery of a pull still running only once", async () => {
		answers = [CHECKING, CHECKING];
		await deliver();
		await deliver();
		expect(await stub().pulls()).toHaveLength(1);
	});

	it("keeps `access` out of a member's reach when the owner maintains it", async () => {
		await call(jack, "POST", path("/members"), { login: "ana" });
		answers = [{ outcome: "nothing" }];
		await deliver({ payload: push({ pusher: { name: "Ana" } }) });
		expect(plans[0].writable).toEqual(["root"]);
		const access = repos.minted.findLast((token) => token.name.endsWith(".t.access"));
		expect(access?.scope).toBe("read");
	});
});

describe("the merge lock", () => {
	it("waits for a merge in flight rather than fighting it, then goes on", async () => {
		const merged = { ...main, root: sha("c") };
		const begun = await call(jack, "POST", path("/branches/main/merge"), { base: main, list: merged });
		expect(begun.status).toBe(200);

		expect((await deliver()).status).toBe(202);
		expect(plans).toEqual([]);
		expect(await advance()).toBe(true);
		expect(plans).toEqual([]);

		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list: merged })).status).toBe(200);
		answers = [{ outcome: "landed", list: { ...merged, root: sha("9") }, commits: 1 }];
		expect(await advance()).toBe(false);
		expect(plans[0].list, "it starts from what the merge left").toEqual(merged);
		expect((await index()).root).toBe(sha("9"));
	});

	it("gives up a wait that runs too long, for a redelivery to start again", async () => {
		await call(jack, "POST", path("/branches/main/merge"), { base: main, list: { ...main, root: sha("c") } });
		await deliver();
		vi.useFakeTimers({ toFake: ["Date"] });
		try {
			vi.setSystemTime(Date.now() + PULL_LIMIT_MS + 1000);
			expect(await advance()).toBe(false);
		} finally {
			vi.useRealTimers();
		}
		expect(await stub().pulls()).toEqual([]);
	});
});

describe("when it goes wrong", () => {
	it("logs a refusal, moves nothing, and lets go of the lock", async () => {
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		answers = [{ outcome: "refused", why: "commit 123 changes `apps/t3code`" }];
		expect((await deliver()).status).toBe(202);
		expect(await index()).toEqual(main);
		expect(await stub().pulls()).toEqual([]);
		expect(logged.mock.calls.flat().join("\n")).toContain("apps/t3code");
		logged.mockRestore();
		const free = await call(jack, "POST", path("/branches/main/merge"), {
			base: main,
			list: { ...main, root: sha("f") },
		});
		expect(free.status).toBe(200);
	});

	it("retries a failure, then gives up loudly, and a redelivery starts over", async () => {
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		answers = Array.from({ length: PULL_ATTEMPTS }, () => new Error("GitHub said 500"));
		await deliver();
		for (let attempt = 1; attempt < PULL_ATTEMPTS - 1; attempt++) expect(await advance()).toBe(true);
		expect(await advance()).toBe(false);
		expect(await stub().pulls()).toEqual([]);
		expect(logged.mock.calls.flat().join("\n")).toContain("GitHub said 500");
		logged.mockRestore();
		expect(await index()).toEqual(main);

		answers = [{ outcome: "landed", list: { ...main, root: sha("c") }, commits: 1 }];
		expect((await deliver()).status).toBe(202);
		expect((await index()).root).toBe(sha("c"));
	});
});

describe("the signature and the repo", () => {
	it("verifies exactly HMAC-SHA256 of the bytes", async () => {
		const body = new TextEncoder().encode("hello").buffer as ArrayBuffer;
		const good = await sign("key", "hello");
		expect(await verifySignature("key", body, good)).toBe(true);
		expect(await verifySignature("other", body, good)).toBe(false);
		expect(await verifySignature("key", body, null)).toBe(false);
		expect(await verifySignature("key", body, good.replace("sha256=", "sha1="))).toBe(false);
	});

	it("reads a target's URL the way an export does", () => {
		for (const url of [
			"Spheroman/newgit",
			"https://github.com/Spheroman/newgit",
			"https://github.com/Spheroman/newgit.git",
			"git@github.com:Spheroman/newgit.git",
		])
			expect(githubRepo(url)).toBe("spheroman/newgit");
		expect(githubRepo("https://gitlab.com/Spheroman/newgit.git")).toBeNull();
		expect(githubRepo("../mirror.git")).toBeNull();
	});
});
