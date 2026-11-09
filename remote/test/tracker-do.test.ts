import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";

import { type ChildMove, type TrackerDO, type TrackerRef, trackerPath } from "../src/tracker-do";

// One Durable Object per tracker path (docs/design/anchoring.md, *Step 8*):
// its heads, its log, an inbox of its children's moves, and the reservations
// a landing across several trackers takes. New tests live in their own file
// per AGENTS.md's "Working in parallel".

const sha = (c: string) => c.repeat(40);

async function project(): Promise<string> {
	const name = `tdo-${crypto.randomUUID().slice(0, 8)}`;
	await env.PROJECTS.getByName(`jack/${name}`).init(name, "jack");
	return `jack/${name}`;
}

const object = (ref: TrackerRef) => env.TRACKERS.getByName(trackerPath(ref));

describe("a tracker's object", () => {
	it("is named by its path", () => {
		expect(trackerPath({ project: "spheroman/newgit", tracker: "root" })).toBe("spheroman/newgit");
		expect(trackerPath({ project: "spheroman/newgit", tracker: "t3code__repos" })).toBe("spheroman/newgit/t3code/repos");
	});

	it("moves a head only by compare-and-swap", async () => {
		const ref = { project: await project(), tracker: "t3code" };
		expect(await object(ref).cas(ref, "main", null, sha("a"), "jack")).toBe(true);
		expect(await object(ref).cas(ref, "main", sha("9"), sha("b"), "jack")).toBe(false);
		expect(await object(ref).head(ref, "main")).toBe(sha("a"));
	});

	it("tells its parent of each move once, however often it is told", async () => {
		const key = await project();
		const root = { project: key, tracker: "root" };
		const child = { project: key, tracker: "t3code" };
		await object(root).cas(root, "main", null, sha("r"), "jack");
		await object(child).cas(child, "main", null, sha("a"), "jack");
		await object(child).cas(child, "main", sha("a"), sha("b"), "jack");
		const pending = await object(root).pending(root, "main");
		expect(pending.map((move) => move.new)).toEqual([sha("b")]);
		// A delivery again (an outbox retried): nothing new.
		const again: ChildMove = {
			branch: "main",
			child: "t3code",
			childSeq: 2,
			old: sha("a"),
			new: sha("b"),
			by: "jack",
			at: new Date().toISOString(),
			via: null,
			token: null,
		};
		await object(root).receive(root, again);
		expect(await object(root).pending(root, "main")).toHaveLength(1);
	});
});

describe("a landing's reservations", () => {
	it("hide a landing until its anchor decides, then show all of it", async () => {
		const key = await project();
		const root = { project: key, tracker: "root" };
		const child = { project: key, tracker: "t3code" };
		await object(child).cas(child, "main", null, sha("a"), "jack");
		await object(root).cas(root, "main", null, sha("r"), "jack");
		const token = "token-1";
		expect(await object(root).prepare(root, "main", token, sha("r"), sha("s"), "root", "jack")).toBe(true);
		expect(await object(child).prepare(child, "main", token, sha("a"), sha("b"), "root", "jack")).toBe(true);
		// Undecided: the heads before, and nothing else may move them.
		expect(await object(child).head(child, "main")).toBe(sha("a"));
		expect(await object(child).cas(child, "main", sha("a"), sha("z"), "ana")).toBe(false);
		expect(await object(root).decide(root, token, "committed")).toBe("committed");
		// Decided: a read of the child makes its move, though it was never committed there.
		expect(await object(child).head(child, "main")).toBe(sha("b"));
		await object(root).commit(root, token);
		expect(await object(root).head(root, "main")).toBe(sha("s"));
		// Its move is the landing's, which root moved in too: nothing to roll up.
		expect(await object(root).pending(root, "main")).toEqual([]);
	});

	it("held past their time are aborted by the read that meets them", async () => {
		const key = await project();
		const root = { project: key, tracker: "root" };
		const child = { project: key, tracker: "t3code" };
		await object(child).cas(child, "main", null, sha("a"), "jack");
		await object(root).cas(root, "main", null, sha("r"), "jack");
		expect(await object(child).prepare(child, "main", "dead", sha("a"), sha("b"), "root", "jack")).toBe(true);
		await runInDurableObject(object(child), (_instance: TrackerDO, state) => {
			state.storage.sql.exec("UPDATE reservations SET until = 0");
		});
		expect(await object(child).head(child, "main")).toBe(sha("a"));
		expect(await object(root).decision(root, "dead"), "the anchor recorded it aborted").toBe("aborted");
		// And a coordinator coming late cannot commit it.
		expect(await object(root).decide(root, "dead", "committed")).toBe("aborted");
		expect(await object(child).cas(child, "main", sha("a"), sha("c"), "ana")).toBe(true);
	});
});
