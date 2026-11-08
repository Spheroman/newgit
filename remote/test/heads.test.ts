import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";

import type { ProjectDO } from "../src/project";

// Each tracker has a head of its own and every move is logged (anchoring
// build step 2, docs/design/anchoring.md). The branch API is unchanged:
// a branch's list is its heads.

const sha = (c: string) => c.repeat(40);
const fresh = () => env.PROJECTS.getByName(`jack/heads-${crypto.randomUUID().slice(0, 8)}`);

describe("per-tracker heads", () => {
	it("are what a new branch's list says, each logged", async () => {
		const project = fresh();
		await runInDurableObject(project, (instance: ProjectDO) => {
			instance.createBranch("main", { root: sha("a"), access: sha("b") }, "jack");
			expect(instance.branch("main")?.list).toEqual({ access: sha("b"), root: sha("a") });
			const log = instance.headLog("main");
			expect(log.map((move) => [move.tracker, move.old, move.new])).toEqual([
				["root", null, sha("a")],
				["access", null, sha("b")],
			]);
			expect(log.every((move) => move.movedBy === "jack")).toBe(true);
		});
	});

	it("move only what a merge changes, and log exactly that", async () => {
		const project = fresh();
		await runInDurableObject(project, (instance: ProjectDO) => {
			const base = { root: sha("a"), access: sha("b"), web: sha("c"), gone: sha("d") };
			instance.createBranch("main", base, "jack");
			const before = instance.headLog("main").at(-1)!.seq;
			const list = { root: sha("1"), access: sha("b"), web: sha("2"), added: sha("3") };
			expect(instance.beginMerge("main", base, "ana", 60, list).ok).toBe(true);
			expect(instance.land("main", base, list, "ana").ok).toBe(true);
			expect(instance.branch("main")?.list).toEqual(list);
			const moves = instance.headLog("main", before);
			expect(moves.map((move) => [move.tracker, move.old, move.new]).sort()).toEqual(
				[
					["added", null, sha("3")],
					["gone", sha("d"), null],
					["root", sha("a"), sha("1")],
					["web", sha("c"), sha("2")],
				].sort(),
			);
			expect(moves.every((move) => move.movedBy === "ana")).toBe(true);
		});
	});

	it("are untouched by a merge that is refused", async () => {
		const project = fresh();
		await runInDurableObject(project, (instance: ProjectDO) => {
			const base = { root: sha("a"), access: sha("b") };
			instance.createBranch("main", base, "jack");
			const logged = instance.headLog("main").length;
			const stale = { root: sha("9"), access: sha("b") };
			// A tracker it moves (`access`) is not where it saw it.
			expect(instance.beginMerge("main", { ...base, access: sha("c") }, "ana", 60, { ...base, access: sha("d") }).ok).toBe(false);
			expect(instance.land("main", stale, { root: sha("1"), access: sha("b") }, "ana").ok).toBe(
				false,
			);
			expect(instance.branch("main")?.list).toEqual(base);
			expect(instance.headLog("main")).toHaveLength(logged);
		});
	});

	it("keep the list a Worker from before them reads in step", async () => {
		const project = fresh();
		await runInDurableObject(project, (instance: ProjectDO, state) => {
			const base = { root: sha("a"), access: sha("b") };
			instance.createBranch("main", base, "jack");
			const list = { root: sha("1"), access: sha("b") };
			instance.beginMerge("main", base, "ana", 60, list);
			instance.land("main", base, list, "ana");
			const row = state.storage.sql
				.exec<{ list: string }>("SELECT list FROM branches WHERE name = 'main'")
				.toArray()[0];
			expect(JSON.parse(row.list)).toEqual(list);
		});
	});

	it("are adopted from the list of a branch made before them", async () => {
		const project = fresh();
		await runInDurableObject(project, (instance: ProjectDO, state) => {
			const list = { root: sha("a"), access: sha("b") };
			instance.createBranch("main", list, "jack");
			// As a project from before heads left it.
			state.storage.sql.exec("DELETE FROM heads");
			state.storage.sql.exec("DELETE FROM head_log");
			(instance as unknown as { adoptLists(): void }).adoptLists();
			expect(instance.branch("main")?.list).toEqual(list);
			const log = instance.headLog("main");
			expect(log).toHaveLength(2);
			expect(log.every((move) => move.movedBy === "" && move.old === null)).toBe(true);
			// Once.
			(instance as unknown as { adoptLists(): void }).adoptLists();
			expect(instance.headLog("main")).toHaveLength(2);
		});
	});
});
