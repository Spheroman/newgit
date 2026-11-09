import { describe, expect, it, vi } from "vitest";

import { PULL_ATTEMPTS } from "../src/webhook";
import { advance, call, deliver, jack, main, path, setAnswers, sha, stub } from "./webhook-fixture";

// What the remote has not written down yet, read without rolling it up
// (`GET /state`): what `newgit status` says, so "not written yet" never reads
// as "out of date", and a webhook pull that failed or was refused is said
// somewhere a person looks, not only in the logs (docs/design/anchoring.md,
// build order step 6). New tests live in their own file per AGENTS.md's
// "Working in parallel".

const state = async () => (await call(jack, "GET", path("/state"))).body;

describe("the remote's state", () => {
	it("counts what is pending without rolling it up, and says when a roll-up failed", async () => {
		expect((await state()).branches.find((branch: { name: string }) => branch.name === "main")).toEqual({
			name: "main",
			pending: {},
		});
		// A head moved below root, its commit not readable here: root cannot
		// roll up to it.
		expect(await stub().moveHead("main", "t3code", main.t3code, sha("9"), "jack")).toBe(true);
		const before = (await state()).branches.find((branch: { name: string }) => branch.name === "main");
		expect(before.pending).toEqual({ root: 1 });
		expect(before.rollupFailed).toBeUndefined();

		const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
		const read = await call(jack, "GET", path("/branches/main"));
		quiet.mockRestore();
		expect(read.body.behind).toBe(true);
		const after = (await state()).branches.find((branch: { name: string }) => branch.name === "main");
		expect(after.pending).toEqual({ root: 1 });
		expect(after.rollupFailed?.why).toBeTruthy();
	});

	it("says a refused pull, and why", async () => {
		const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
		setAnswers([{ outcome: "refused", why: "commit 123 changes `apps/t3code`" }]);
		await deliver();
		quiet.mockRestore();
		const ended = (await state()).ended;
		expect(ended.at(-1)).toMatchObject({ target: "github", outcome: "refused", why: "commit 123 changes `apps/t3code`" });
	});

	it("says why a pull is failing while it retries, and that it gave up", async () => {
		const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
		setAnswers(Array.from({ length: PULL_ATTEMPTS }, () => new Error("GitHub said 500")));
		await deliver();
		const waiting = (await state()).pulls;
		expect(waiting).toHaveLength(1);
		expect(waiting[0].lastError).toContain("GitHub said 500");
		for (let attempt = 1; attempt < PULL_ATTEMPTS; attempt++) await advance();
		quiet.mockRestore();
		const now = await state();
		expect(now.pulls).toEqual([]);
		expect(now.ended.at(-1)).toMatchObject({ outcome: "gave up", why: expect.stringContaining("GitHub said 500") });
	});

	it("says a pull that landed", async () => {
		setAnswers([{ outcome: "landed", list: { ...main, root: sha("c") }, commits: 1 }]);
		await deliver();
		expect((await state()).ended.at(-1)).toMatchObject({ outcome: "landed" });
	});
});
