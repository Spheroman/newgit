import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it, vi } from "vitest";

import { CHECKING, type MirrorInstance } from "../src/endpoint";
import { PULL_ATTEMPTS, implicated } from "../src/webhook";
import {
	advance,
	call,
	deliver,
	index,
	jack,
	main,
	path,
	peeks,
	plans,
	push,
	repos,
	setAnswers,
	setLooks,
	sha,
	stub,
} from "./webhook-fixture";

// Follow-ups to the webhook: a look before the lock, one driver for the
// queue, a stale base started over, and instances whose work the mirror never
// saw. New tests live in their own file per AGENTS.md's "Working in parallel".

const merged = { ...main, root: sha("f") };
const startMerge = () => call(jack, "POST", path("/branches/main/merge"), { base: main, list: merged });

describe("an event for commits from here", () => {
	it("takes no lock", async () => {
		setLooks([{ outcome: "nothing" }]);
		expect((await deliver()).status).toBe(202);
		expect(peeks).toHaveLength(1);
		expect(plans, "the landing never ran").toEqual([]);
		expect(await stub().pulls()).toEqual([]);
		expect((await startMerge()).status).toBe(200);
	});

	it("leaves `newgit merge` free while the look runs, however long", async () => {
		setLooks([CHECKING, CHECKING, { outcome: "nothing" }]);
		await deliver();
		expect((await startMerge()).status).toBe(200);
		await advance();
		await advance();
		expect(await stub().pulls()).toEqual([]);
		expect(plans).toEqual([]);
	});

	it("looks with read passwords, and cancels them", async () => {
		setLooks([{ outcome: "nothing" }]);
		await deliver();
		const looked = repos.minted.slice(-3);
		expect(looked.map((token) => token.scope)).toEqual(["read", "read", "read"]);
		for (const token of looked) expect(repos.revokedTokens.has(token.id)).toBe(true);
	});
});

describe("the queue has one driver", () => {
	it("the route only queues", async () => {
		expect((await deliver({ hold: true })).status).toBe(202);
		expect(peeks).toEqual([]);
		expect(plans).toEqual([]);
		expect(await stub().pulls()).toHaveLength(1);
		const alarm = await runInDurableObject(stub(), (_instance, state) => state.storage.getAlarm());
		expect(alarm, "the alarm is what runs it").not.toBeNull();
	});
});

describe("ProjectDO.alarm()", () => {
	// The alarm builds the real deps, and tests have no Artifacts or
	// container, so a pull fails at its first step: which is what shows the
	// alarm reaches `advancePulls`, counts the failure, and keeps itself going.
	it("does nothing with an empty queue, and does not re-arm", async () => {
		expect(await runDurableObjectAlarm(stub())).toBe(false);
		await runInDurableObject(stub(), async (instance) => instance.alarm());
		expect(await runInDurableObject(stub(), (_instance, state) => state.storage.getAlarm())).toBeNull();
	});

	it("moves the queue on, re-arms while there is work, and stops", async () => {
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		await deliver({ hold: true });
		expect(await runDurableObjectAlarm(stub())).toBe(true);
		expect((await stub().pulls())[0].attempts).toBe(1);
		for (let attempt = 2; attempt < PULL_ATTEMPTS; attempt++) expect(await runDurableObjectAlarm(stub())).toBe(true);
		expect(await runDurableObjectAlarm(stub())).toBe(true);
		expect(await stub().pulls()).toEqual([]);
		expect(await runDurableObjectAlarm(stub()), "nothing left to wake for").toBe(false);
		logged.mockRestore();
	});
});

describe("`main` not being what the pull began from", () => {
	it("starts again from `main` rather than retrying a stale base", async () => {
		await deliver({ hold: true });
		setAnswers([
			{ outcome: "moved", why: "a merge is landing" },
			{ outcome: "landed", list: { ...merged, root: sha("9") }, commits: 1 },
		]);
		expect(await advance()).toBe(true);
		const [waiting] = await stub().pulls();
		expect(waiting.attempts, "not a failure").toBe(0);
		expect(waiting.base, "the base is forgotten").toBeUndefined();

		// `main` moves meanwhile, and the next step begins from there.
		await stub().releaseMerge("main", "the GitHub webhook");
		await startMerge();
		expect((await call(jack, "PUT", path("/branches/main"), { base: main, list: merged })).status).toBe(200);
		expect(await advance()).toBe(false);
		expect(plans.map((plan) => plan.list.root)).toEqual([sha("a"), sha("f")]);
		expect((await index()).root).toBe(sha("9"));
	});
});

describe("work only an instance holds", () => {
	const instance = (name: string): MirrorInstance => ({ name, pins: { t3code: sha("7") } });

	it("names the instance a merge commit says it came from", () => {
		const all = [instance("feature-x"), instance("other")];
		expect(implicated("Merge pull request #7 from Spheroman/feature/x\n\nbody", all)).toEqual([all[0]]);
		expect(implicated("Merge pull request #7 from Spheroman/unknown", all)).toEqual([]);
	});

	it("counts every instance when the commit does not say: a squash", () => {
		const all = [instance("feature-x"), instance("other")];
		expect(implicated("A change (#7)", all)).toEqual(all);
		expect(implicated(undefined, all)).toEqual(all);
	});

	it("hands the container the instance's withheld pins from its last checkpoint", async () => {
		await call(jack, "POST", path("/instances"), { name: "feature", base_rev: "x" });
		await stub().startFork("feature", "t3code");
		await stub().completeFork("feature", "t3code");
		await call(jack, "PUT", path("/checkpoints/feature/one"), { record: { code: { head: { t3code: sha("7") } } } });
		setLooks([{ outcome: "refused", why: "run `newgit merge feature`, then redeliver this event" }]);
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		await deliver({
			payload: push({ head_commit: { message: "Merge pull request #7 from Spheroman/feature" } }),
		});
		expect(peeks[0].instances).toEqual([{ name: "feature", pins: { t3code: sha("7") } }]);
		expect(logged.mock.calls.flat().join("\n")).toContain("newgit merge feature");
		logged.mockRestore();
		expect(await index()).toEqual(main);
		expect((await startMerge()).status, "nothing locked").toBe(200);
	});

	it("reads a fork with no checkpoint as work", async () => {
		await call(jack, "POST", path("/instances"), { name: "feature", base_rev: "x" });
		await stub().startFork("feature", "t3code");
		await stub().completeFork("feature", "t3code");
		setLooks([{ outcome: "nothing" }]);
		await deliver();
		expect(peeks[0].instances).toEqual([{ name: "feature", pins: { t3code: "" } }]);
	});

	it("ignores an instance that forked only what the mirror ships", async () => {
		await call(jack, "POST", path("/instances"), { name: "feature", base_rev: "x" });
		await stub().startFork("feature", "root");
		await stub().completeFork("feature", "root");
		setLooks([{ outcome: "nothing" }]);
		await deliver();
		expect(peeks[0].instances).toEqual([]);
	});
});

