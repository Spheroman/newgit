import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it, vi } from "vitest";

import { CHECKING } from "../src/endpoint";
import { PULLS_ASKED, PULL_ATTEMPTS } from "../src/webhook";
import {
	THEIRS,
	advance,
	asked,
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
	setGitHub,
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

/** A pull request from this repo's branch `ref`. */
const pullRequest = (ref: string, repo = "Spheroman/newgit") => ({ head: { ref, repo: { full_name: repo } } });

/** An instance with a fork of `tracker`, and a checkpoint saying what it holds. */
async function instanceWith(instance: string, tracker = "t3code") {
	await call(jack, "POST", path("/instances"), { name: instance, base_rev: "x" });
	await stub().startFork(instance, tracker);
	await stub().completeFork(instance, tracker);
	await call(jack, "PUT", path(`/checkpoints/${instance}/one`), { record: { code: { head: { [tracker]: sha("7") } } } });
}

const holding = (instance: string) => ({ name: instance, pins: { t3code: sha("7") } });
const landed = { outcome: "landed", list: merged, commits: 1 } as const;

describe("whose withheld work a commit may hold", () => {
	it("lands an unrelated squash while another instance holds withheld work", async () => {
		await instanceWith("feature");
		setGitHub(() => Response.json([pullRequest("other")]));
		setAnswers([landed]);
		await deliver();
		expect(asked).toEqual([THEIRS]);
		expect(plans[0].instances).toEqual([]);
		expect((await index()).root).toBe(sha("f"));
	});

	it("checks the instance whose squash this is", async () => {
		await instanceWith("feature");
		await instanceWith("other");
		setGitHub(() => Response.json([pullRequest("feature")]));
		setAnswers([{ outcome: "refused", why: "run `newgit merge feature`, then redeliver this event" }]);
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		await deliver();
		expect(plans[0].instances, "only that instance").toEqual([holding("feature")]);
		expect(logged.mock.calls.flat().join("\n")).toContain("newgit merge feature");
		logged.mockRestore();
		expect(await index()).toEqual(main);
		expect((await startMerge()).status, "nothing locked").toBe(200);
	});

	it("matches a branch to its instance's slug", async () => {
		await instanceWith("feature-x");
		setGitHub(() => Response.json([pullRequest("Feature/X")]));
		await deliver();
		expect(plans[0].instances).toEqual([holding("feature-x")]);
	});

	it("lands a direct push: no pull request, no instance", async () => {
		await instanceWith("feature");
		setAnswers([landed]);
		await deliver();
		expect(asked).toEqual([THEIRS]);
		expect(plans[0].instances).toEqual([]);
		expect((await index()).root).toBe(sha("f"));
	});

	it("does not take a fork's branch for an instance of the same name", async () => {
		await instanceWith("feature");
		setGitHub(() => Response.json([pullRequest("feature", "Someone/newgit")]));
		await deliver();
		expect(plans[0].instances).toEqual([]);
	});

	it("counts a pull request not marked merged yet: the event can beat the mark", async () => {
		await instanceWith("feature");
		setGitHub(() => Response.json([{ ...pullRequest("feature"), merged_at: null }]));
		await deliver();
		expect(plans[0].instances).toEqual([holding("feature")]);
	});

	it("asks about every commit the event landed", async () => {
		await instanceWith("feature");
		setGitHub((commit) => Response.json(commit === sha("c") ? [pullRequest("feature")] : []));
		await deliver({ payload: push({ commits: [{ id: sha("c") }, { id: THEIRS }] }) });
		expect([...asked].sort()).toEqual([THEIRS, sha("c")].sort());
		expect(plans[0].instances).toEqual([holding("feature")]);
	});

	it("takes a merge commit's subject at its word, and spends no call", async () => {
		await instanceWith("feature");
		await instanceWith("other");
		await deliver({ payload: push({ head_commit: { message: "Merge pull request #7 from Spheroman/feature\n\nbody" } }) });
		expect(asked).toEqual([]);
		expect(plans[0].instances).toEqual([holding("feature")]);
	});

	it("spends no call when no instance holds withheld work", async () => {
		await instanceWith("shipped", "root");
		await deliver();
		expect(asked).toEqual([]);
		expect(plans[0].instances).toEqual([]);
	});

	it("spends no call on an event for our own export", async () => {
		await instanceWith("feature");
		setLooks([{ outcome: "nothing" }]);
		await deliver();
		expect(asked).toEqual([]);
		expect(plans).toEqual([]);
	});

	it("reads a fork with no checkpoint as work", async () => {
		await call(jack, "POST", path("/instances"), { name: "feature", base_rev: "x" });
		await stub().startFork("feature", "t3code");
		await stub().completeFork("feature", "t3code");
		setGitHub(() => Response.json([pullRequest("feature")]));
		await deliver();
		expect(plans[0].instances).toEqual([{ name: "feature", pins: { t3code: "" } }]);
	});
});

describe("when GitHub cannot say", () => {
	const failing: [string, () => Response][] = [
		["a rate limit", () => new Response("rate limited", { status: 403 })],
		["a 5xx", () => new Response("down", { status: 503 })],
		["a private repo", () => new Response("not found", { status: 404 })],
		[
			"no connection",
			() => {
				throw new Error("the network went away");
			},
		],
	];

	it.each(failing)("counts every instance on %s, says why, and a redelivery can land it", async (_case, answer) => {
		await instanceWith("feature");
		await instanceWith("other");
		setGitHub(answer);
		setAnswers([{ outcome: "refused", why: "may hold `feature`'s work: redeliver this event" }]);
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		await deliver();
		expect(plans[0].instances).toEqual([holding("feature"), holding("other")]);
		expect(logged.mock.calls.flat().join("\n")).toContain("GitHub could not say");
		logged.mockRestore();
		expect(await index()).toEqual(main);
		expect(await stub().pulls(), "finished, so a redelivery starts over").toEqual([]);

		setGitHub(() => Response.json([pullRequest("nobody")]));
		setAnswers([landed]);
		await deliver();
		expect(plans[1].instances).toEqual([]);
		expect((await index()).root).toBe(sha("f"));
	});

	it("does not ask about more commits than it may", async () => {
		await instanceWith("feature");
		const logged = vi.spyOn(console, "error").mockImplementation(() => {});
		const commits = Array.from({ length: PULLS_ASKED + 1 }, (_, n) => ({ id: n.toString(16).padStart(40, "0") }));
		await deliver({ payload: push({ commits }) });
		logged.mockRestore();
		expect(asked).toEqual([]);
		expect(plans[0].instances).toEqual([holding("feature")]);
	});
});
