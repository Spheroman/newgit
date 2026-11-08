import { env } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";

import { commitObject, hashObject, packObjects, treeObject } from "../src/objects";
import type { ProjectDO } from "../src/project";
import { ROLLUP_SIGNATURE, type RollupStore, rollUp } from "../src/rollup";
import { nativeIndex } from "../src/walk";
import { FakeRepos } from "./fake-repos";

// Rolling a parent up in the Worker (docs/design/anchoring.md, build order
// step 4): the commits that write a child's new head into its parent's
// `.newgit/pins/`, made from Artifacts' reads, encoded as Git encodes them,
// and landed by one compare-and-swap. New tests live in their own file per
// AGENTS.md's "Working in parallel".

const sha = (c: string) => c.repeat(40);
const blob = "45b983be36b73c0788dc9cbcb76cbb80fc7bb057";

describe("objects made in the Worker", () => {
	it("hash as Git's own do", async () => {
		// Each expected id is what `git mktree` / `git commit-tree` made.
		const pins = await treeObject([{ name: "t3code", hash: sha("a"), type: "gitlink" }]);
		expect(pins.hash).toBe("c7aabcf5cf204b8bfbadb376732bb0167755f620");
		const two = await treeObject([
			{ name: "t3code", hash: sha("a"), type: "gitlink" },
			{ name: "access", hash: sha("b"), type: "gitlink" },
		]);
		expect(two.hash).toBe("da5ac15d71f58ae3f29a2f17d67cd73829e5a9b3");
		const newgit = await treeObject([{ name: "pins", hash: two.hash, type: "tree" }]);
		expect(newgit.hash).toBe("7c3a0395fea9ea4b07c1a95e767eb0b8ea40652c");
		// Out of order on purpose: a tree named `a` sorts after `a.txt`.
		const root = await treeObject([
			{ name: "a", hash: newgit.hash, type: "tree" },
			{ name: "run.sh", hash: blob, type: "exec" },
			{ name: "a.txt", hash: blob, type: "blob" },
			{ name: "link", hash: blob, type: "symlink" },
			{ name: ".newgit", hash: newgit.hash, type: "tree" },
			{ name: "a.b", hash: blob, type: "blob" },
		]);
		expect(root.hash).toBe("7e4b1f6968dd9ccc745bbd0d95c7d2f57145ae6c");
		const at = 1794000000;
		const commit = await commitObject({
			tree: pins.hash,
			parents: ["27e80061420c618b68c49c5c8592d1cb5a18ba10", "18ea4d49ff525c8589887583009e7bcf9507e5b8"],
			author: { ...ROLLUP_SIGNATURE, at },
			committer: { ...ROLLUP_SIGNATURE, at },
			message: "Roll up `t3code`",
		});
		expect(commit.hash).toBe("7e2788b30674ea917886a2888cadf8fae1191903");
	});

	it("pack into what Git reads: its header, each object deflated, its checksum", async () => {
		const pins = await treeObject([{ name: "t3code", hash: sha("a"), type: "gitlink" }]);
		const pack = await packObjects([pins]);
		expect(new TextDecoder().decode(pack.subarray(0, 4))).toBe("PACK");
		expect([...pack.subarray(4, 12)]).toEqual([0, 0, 0, 2, 0, 0, 0, 1]);
		// A tree (type 2) of 34 bytes: 0x80 | 2 << 4 | 34 & 15, then 34 >> 4.
		expect([...pack.subarray(12, 14)]).toEqual([0x80 | (2 << 4) | (34 & 15), 34 >> 4]);
		const deflated = pack.subarray(14, pack.length - 20);
		const inflated = new Uint8Array(
			await new Response(new Blob([deflated]).stream().pipeThrough(new DecompressionStream("deflate"))).arrayBuffer(),
		);
		expect(await hashObject("tree", inflated)).toBe(pins.hash);
		const trailer = new Uint8Array(await crypto.subtle.digest("SHA-1", pack.subarray(0, pack.length - 20)));
		expect([...pack.subarray(pack.length - 20)]).toEqual([...trailer]);
	});
});

let repos: FakeRepos;
let project: DurableObjectStub<ProjectDO>;
const repoOf = (tracker: string) => `repo-${tracker}`;

const store = (): RollupStore => ({
	head: (branch, tracker) => project.head(branch, tracker),
	trackers: async (branch) => Object.keys((await project.branch(branch))?.list ?? {}),
	pending: (branch, parent) => project.pending(branch, parent),
	rolledUp: (branch, parent, old, next, through) => project.rolledUp(branch, parent, old, next, through),
	wasHead: (branch, tracker, commit) => project.wasHead(branch, tracker, commit),
});

/** A tracker's commit: a file of its own, and `pins` at `.newgit/pins/`. */
async function trackerCommit(tracker: string, pins: Record<string, string>, parents: string[] = [], message = "work") {
	const pinsTree = await treeObject(Object.entries(pins).map(([name, hash]) => ({ name, hash, type: "gitlink" as const })));
	const newgit = await treeObject([{ name: "pins", hash: pinsTree.hash, type: "tree" }]);
	const tree = await treeObject([
		{ name: "README.md", hash: blob, type: "blob" },
		{ name: ".newgit", hash: newgit.hash, type: "tree" },
	]);
	const at = 1_790_000_000 + parents.length;
	const commit = await commitObject({
		tree: tree.hash,
		parents,
		author: { name: "jack", email: "jack@example.com", at },
		committer: { name: "jack", email: "jack@example.com", at },
		message,
	});
	await repos.updateRefs(repoOf(tracker), [], [pinsTree, newgit, tree, commit]);
	return commit.hash;
}

/** `list` matches what its root commit pins, all the way down. */
async function consistent(): Promise<void> {
	const list = (await project.branch("main"))!.list;
	expect(await nativeIndex(repos, repoOf, list)).toEqual({ matches: true, actual: list });
}

beforeEach(() => {
	repos = new FakeRepos();
	project = env.PROJECTS.getByName(`jack/rollup-${crypto.randomUUID().slice(0, 8)}`);
});

describe("rolling up", () => {
	it("writes a child's new head into its parent, once", async () => {
		const access = await trackerCommit("access", {});
		const a = await trackerCommit("t3code", {});
		const root = await trackerCommit("root", { t3code: a, access });
		await project.createBranch("main", { root, t3code: a, access }, "jack");
		const b = await trackerCommit("t3code", {}, [a]);
		expect(await project.moveHead("main", "t3code", a, b, "ana")).toBe(true);

		const rolled = await rollUp(repos, store(), repoOf, "main", "root");
		expect(rolled.written).toBe(1);
		expect(rolled.head).not.toBe(root);
		expect(await project.head("main", "root")).toBe(rolled.head);
		await consistent();
		const made = await repos.readCommit(repoOf("root"), rolled.head!);
		expect(made?.parents).toEqual([root]);
		// The parent's own files are as they were: only the pin moved.
		const tree = await repos.readTree(repoOf("root"), made!.tree);
		expect(tree?.find((entry) => entry.name === "README.md")?.hash).toBe(blob);

		const again = await rollUp(repos, store(), repoOf, "main", "root");
		expect(again).toEqual({ head: rolled.head, written: 0 });
		expect(await project.pending("main", "root")).toEqual([]);
	});

	it("rolls the deepest up first: a grandchild's move reaches root through its parent", async () => {
		const r0 = await trackerCommit("t3code__repos", {});
		const a = await trackerCommit("t3code", { repos: r0 });
		const root = await trackerCommit("root", { t3code: a });
		await project.createBranch("main", { root, t3code: a, t3code__repos: r0 }, "jack");
		const r1 = await trackerCommit("t3code__repos", {}, [r0]);
		await project.moveHead("main", "t3code__repos", r0, r1, "ana");

		const rolled = await rollUp(repos, store(), repoOf, "main", "root");
		expect(rolled.written).toBe(2);
		expect(await project.head("main", "t3code")).not.toBe(a);
		await consistent();
	});

	it("writes one commit per change, in order", async () => {
		const a = await trackerCommit("t3code", {});
		const x = await trackerCommit("access", {});
		const root = await trackerCommit("root", { t3code: a, access: x });
		await project.createBranch("main", { root, t3code: a, access: x }, "jack");
		const b = await trackerCommit("t3code", {}, [a]);
		const y = await trackerCommit("access", {}, [x]);
		await project.moveHead("main", "t3code", a, b, "ana");
		await project.moveHead("main", "access", x, y, "jack");

		const rolled = await rollUp(repos, store(), repoOf, "main", "root");
		expect(rolled.written).toBe(2);
		const last = await repos.readCommit(repoOf("root"), rolled.head!);
		const first = await repos.readCommit(repoOf("root"), last!.parents[0]);
		expect(first?.parents).toEqual([root]);
		await consistent();
	});

	it("keeps a change's own commit in the parent: fast-forwarded when it sits on the head", async () => {
		const a = await trackerCommit("t3code", {});
		const root = await trackerCommit("root", { t3code: a });
		await project.createBranch("main", { root, t3code: a }, "jack");
		const b = await trackerCommit("t3code", {}, [a]);
		// What `split` made for the change: root's commit pinning `b`, on `root`.
		const theirs = await trackerCommit("root", { t3code: b }, [root], "Change t3code");
		await project.moveHead("main", "t3code", a, b, "ana", theirs);

		const rolled = await rollUp(repos, store(), repoOf, "main", "root");
		expect(rolled.head, "its id and signature, as it was made").toBe(theirs);
		expect(rolled.written).toBe(0);
		await consistent();
	});

	it("merges a change's own commit when the head moved on since it was made", async () => {
		const a = await trackerCommit("t3code", {});
		const x = await trackerCommit("access", {});
		const root = await trackerCommit("root", { t3code: a, access: x });
		await project.createBranch("main", { root, t3code: a, access: x }, "jack");
		const y = await trackerCommit("access", {}, [x]);
		await project.moveHead("main", "access", x, y, "jack");
		const b = await trackerCommit("t3code", {}, [a]);
		const theirs = await trackerCommit("root", { t3code: b, access: x }, [root], "Change t3code");
		await project.moveHead("main", "t3code", a, b, "ana", theirs);

		const rolled = await rollUp(repos, store(), repoOf, "main", "root");
		const merge = await repos.readCommit(repoOf("root"), rolled.head!);
		expect(merge?.parents[1], "the change's commit is a parent").toBe(theirs);
		await consistent();
	});

	it("races to the same head: one wins the swap, the other finds nothing left", async () => {
		const a = await trackerCommit("t3code", {});
		const root = await trackerCommit("root", { t3code: a });
		await project.createBranch("main", { root, t3code: a }, "jack");
		const b = await trackerCommit("t3code", {}, [a]);
		await project.moveHead("main", "t3code", a, b, "ana");

		const [one, two] = await Promise.all([
			rollUp(repos, store(), repoOf, "main", "root"),
			rollUp(repos, store(), repoOf, "main", "root"),
		]);
		expect(one.head).toBe(two.head);
		expect(await project.head("main", "root")).toBe(one.head);
		await consistent();
	});

	it("leaves nothing pending after a list lands whole", async () => {
		const a = await trackerCommit("t3code", {});
		const root = await trackerCommit("root", { t3code: a });
		await project.createBranch("main", { root, t3code: a }, "jack");
		const b = await trackerCommit("t3code", {}, [a]);
		const moved = await trackerCommit("root", { t3code: b }, [root]);
		const base = { root, t3code: a };
		const list = { root: moved, t3code: b };
		expect((await project.beginMerge("main", base, "jack", 60, list)).ok).toBe(true);
		expect((await project.land("main", base, list, "jack")).ok).toBe(true);
		expect(await project.pending("main", "root")).toEqual([]);
		expect((await rollUp(repos, store(), repoOf, "main", "root")).head).toBe(moved);
	});

	it("refuses a head move that is not from where the head is", async () => {
		const a = await trackerCommit("t3code", {});
		const root = await trackerCommit("root", { t3code: a });
		await project.createBranch("main", { root, t3code: a }, "jack");
		expect(await project.moveHead("main", "t3code", sha("9"), sha("8"), "ana")).toBe(false);
		expect(await project.head("main", "t3code")).toBe(a);
	});
});
