import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";

import type { ProjectDO } from "../src/project";

// Each tracker has a head of its own and every move is logged (anchoring
// build steps 2 and 8, docs/design/anchoring.md): heads live in each
// tracker's own object now (`TrackerDO`, by path), and `ProjectDO` asks them.
// The branch API is unchanged: a branch's list is its heads.

const sha = (c: string) => c.repeat(40);
const fresh = async () => {
	const name = `heads-${crypto.randomUUID().slice(0, 8)}`;
	const project = env.PROJECTS.getByName(`jack/${name}`);
	await project.init(name, "jack");
	return project;
};

describe("per-tracker heads", () => {
	it("are what a new branch's list says, each logged", async () => {
		const project = await fresh();
		await project.createBranch("main", { root: sha("a"), access: sha("b") }, "jack");
		expect((await project.branch("main"))?.list).toEqual({ access: sha("b"), root: sha("a") });
		const log = await project.headLog("main");
		expect(log.map((move) => [move.tracker, move.old, move.new]).sort()).toEqual(
			[
				["root", null, sha("a")],
				["access", null, sha("b")],
			].sort(),
		);
		expect(log.every((move) => move.movedBy === "jack")).toBe(true);
	});

	it("move only what a merge changes, and log exactly that", async () => {
		const project = await fresh();
		const base = { root: sha("a"), access: sha("b"), web: sha("c"), gone: sha("d") };
		await project.createBranch("main", base, "jack");
		const list = { root: sha("1"), access: sha("b"), web: sha("2"), added: sha("3") };
		expect((await project.beginMerge("main", base, "ana", 60, list)).ok).toBe(true);
		expect((await project.land("main", base, list, "ana")).ok).toBe(true);
		expect((await project.branch("main"))?.list).toEqual(list);
		// The dropped tracker's log is its own; what the branch lists now logged these.
		const moves = (await project.headLog("main")).filter((move) => move.movedBy === "ana");
		expect(moves.map((move) => [move.tracker, move.old, move.new]).sort()).toEqual(
			[
				["added", null, sha("3")],
				["root", sha("a"), sha("1")],
				["web", sha("c"), sha("2")],
			].sort(),
		);
		expect(moves.every((move) => move.movedBy === "ana")).toBe(true);
	});

	it("are untouched by a merge that is refused", async () => {
		const project = await fresh();
		const base = { root: sha("a"), access: sha("b") };
		await project.createBranch("main", base, "jack");
		const logged = (await project.headLog("main")).length;
		const stale = { root: sha("9"), access: sha("b") };
		// A tracker it moves (`access`) is not where it saw it.
		expect(
			(await project.beginMerge("main", { ...base, access: sha("c") }, "ana", 60, { ...base, access: sha("d") })).ok,
		).toBe(false);
		expect((await project.land("main", stale, { root: sha("1"), access: sha("b") }, "ana")).ok).toBe(false);
		expect((await project.branch("main"))?.list).toEqual(base);
		expect(await project.headLog("main")).toHaveLength(logged);
	});

	it("keep the list of which trackers a branch has in step", async () => {
		const project = await fresh();
		const base = { root: sha("a"), access: sha("b") };
		await project.createBranch("main", base, "jack");
		const list = { root: sha("1"), access: sha("b"), added: sha("2") };
		await project.beginMerge("main", base, "ana", 60, list);
		await project.land("main", base, list, "ana");
		await runInDurableObject(project, (_instance: ProjectDO, state) => {
			const row = state.storage.sql.exec<{ list: string }>("SELECT list FROM branches WHERE name = 'main'").toArray()[0];
			expect(Object.keys(JSON.parse(row.list)).sort()).toEqual(["access", "added", "root"]);
		});
	});

	it("are handed over from a project from before trackers had objects, once", async () => {
		const project = await fresh();
		const list = { root: sha("a"), access: sha("b") };
		// As an older remote left it: heads in the project's own table only.
		await runInDurableObject(project, (_instance: ProjectDO, state) => {
			const now = new Date().toISOString();
			state.storage.sql.exec("INSERT INTO branches VALUES ('main', ?, 'jack', ?, ?)", JSON.stringify(list), now, now);
			for (const [tracker, commit] of Object.entries(list))
				state.storage.sql.exec("INSERT INTO heads VALUES ('main', ?, ?)", tracker, commit);
		});
		expect((await project.branch("main"))?.list).toEqual(list);
		// The trackers' own objects hold them now: a move there is the truth.
		expect(await project.moveHead("main", "access", sha("b"), sha("c"), "jack")).toBe(true);
		expect((await project.branch("main"))?.list).toEqual({ ...list, access: sha("c") });
	});
});

describe("a landing across several trackers", () => {
	it("moves all of them or none", async () => {
		const project = await fresh();
		const base = { root: sha("a"), t3code: sha("b"), t3code__repos: sha("c") };
		await project.createBranch("main", base, "jack");
		// Someone moved `t3code__repos` meanwhile: the landing finds it moved,
		// and neither root nor t3code moves.
		expect(await project.moveHead("main", "t3code__repos", sha("c"), sha("9"), "ana")).toBe(true);
		const list = { root: sha("1"), t3code: sha("2"), t3code__repos: sha("3") };
		await project.beginMerge("main", { ...base, t3code__repos: sha("9") }, "jack", 60, list);
		const landed = await project.land("main", base, list, "jack");
		expect(landed.ok).toBe(false);
		const now = (await project.branch("main"))!.list;
		expect(now.root).toBe(sha("a"));
		expect(now.t3code).toBe(sha("b"));
	});

	it("leaves no reservation behind, so the next landing goes through", async () => {
		const project = await fresh();
		const base = { root: sha("a"), t3code: sha("b") };
		await project.createBranch("main", base, "jack");
		const list = { root: sha("1"), t3code: sha("2") };
		await project.beginMerge("main", base, "jack", 60, list);
		expect((await project.land("main", base, list, "jack")).ok).toBe(true);
		expect((await project.branch("main"))!.list).toEqual(list);
		expect(await project.moveHead("main", "t3code", sha("2"), sha("3"), "jack")).toBe(true);
	});
});
