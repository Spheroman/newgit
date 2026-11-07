import { env } from "cloudflare:workers";
import { describe, expect, it, vi } from "vitest";

import { CHECKING, pullKey } from "../src/endpoint";
import { type Deps, handle, issue } from "../src/index";
import { PULL_ATTEMPTS, PULL_LIMIT_MS, githubRepo, verifySignature } from "../src/webhook";
import {
	SECRET,
	THEIRS,
	advance,
	call,
	deliver,
	index,
	jack,
	main,
	name,
	path,
	plans,
	push,
	repos,
	setAnswers,
	setLooks,
	sha,
	sign,
	stub,
} from "./webhook-fixture";

// A pull request merged on a mirror comes back through GitHub's webhook: a
// signed push event, a pull in the endpoint's container, and `main` moved the
// way a merge moves it. The container is faked here; `newgit endpoint-pull`
// has its tests in crates/newgit-core/tests/endpoint_pull.rs.
// New tests live in their own file per AGENTS.md's "Working in parallel".

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
		setAnswers([{ outcome: "landed", list: moved, commits: 2 }]);
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

	it("waits for the container, starting it once, and holds nothing meanwhile", async () => {
		setAnswers([CHECKING, CHECKING, { outcome: "landed", list: { ...main, root: sha("c") }, commits: 1 }]);
		await deliver();
		expect(await advance()).toBe(true);
		expect(await index()).toEqual(main);
		const moved = { ...main, root: sha("f") };
		const during = await call(jack, "POST", path("/branches/main/merge"), { base: main, list: moved });
		expect(during.status, "a merge may begin beside the pull").toBe(200);
		expect(await advance()).toBe(false);
		expect((await index()).root).toBe(sha("c"));
		expect(new Set(plans.map(pullKey)).size).toBe(1);
		// The pull landed first, so the merge finds `main` moved.
		const late = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved });
		expect(late.status).toBe(409);
	});

	it("does nothing on a redelivery, or on an event for our own export", async () => {
		setLooks([{ outcome: "nothing" }, { outcome: "nothing" }]);
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
		setAnswers([CHECKING, CHECKING]);
		await deliver({ hold: true });
		await deliver({ hold: true });
		expect(await stub().pulls()).toHaveLength(1);
	});

	it("keeps `access` out of a member's reach when the owner maintains it", async () => {
		await call(jack, "POST", path("/members"), { login: "ana" });
		setAnswers([{ outcome: "nothing" }]);
		await deliver({ payload: push({ pusher: { name: "Ana" } }) });
		expect(plans[0].writable).toEqual(["root"]);
		const access = repos.minted.findLast((token) => token.name.endsWith(".t.access"));
		expect(access?.scope).toBe("read");
	});
});

describe("a merge beside a pull", () => {
	it("lands first, and the pull starts again from what it left", async () => {
		const merged = { ...main, root: sha("c") };
		const begun = await call(jack, "POST", path("/branches/main/merge"), { base: main, list: merged });
		expect(begun.status).toBe(200);

		setAnswers([CHECKING, { outcome: "landed", list: { ...main, root: sha("8") }, commits: 1 }]);
		expect((await deliver()).status).toBe(202);
		expect(plans.at(-1)?.list, "the pull runs beside the merge").toEqual(main);

		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list: merged })).status).toBe(200);
		expect(await advance(), "its landing found `main` moved").toBe(true);
		expect((await index()).root).toBe(sha("c"));
		setAnswers([{ outcome: "landed", list: { ...merged, root: sha("9") }, commits: 1 }]);
		expect(await advance()).toBe(false);
		expect(plans.at(-1)?.list, "it starts from what the merge left").toEqual(merged);
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
		setAnswers([{ outcome: "refused", why: "commit 123 changes `apps/t3code`" }]);
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
		setAnswers(Array.from({ length: PULL_ATTEMPTS }, () => new Error("GitHub said 500")));
		await deliver();
		for (let attempt = 1; attempt < PULL_ATTEMPTS - 1; attempt++) expect(await advance()).toBe(true);
		expect(await advance()).toBe(false);
		expect(await stub().pulls()).toEqual([]);
		expect(logged.mock.calls.flat().join("\n")).toContain("GitHub said 500");
		logged.mockRestore();
		expect(await index()).toEqual(main);

		setAnswers([{ outcome: "landed", list: { ...main, root: sha("c") }, commits: 1 }]);
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
