// Rolling a parent up: writing down what its children's heads already say.
//
// The log is the truth and commits are checkpoints of it
// (docs/design/anchoring.md). A change lands at its anchor; the trackers
// above it only learn of it here, when something needs them to. Rolling up
// never merges files: below the anchor it moves only pins, and a pin's new
// value is that child's head. So a roll-up is the parent's tree with entries
// under `.newgit/pins/` replaced, one commit per change (`history =
// commits`, the default), made in the Worker from Artifacts' reads and
// pushed as a pack (`objects.ts`), then one compare-and-swap of the
// parent's head. Deepest first: a child with its own children rolls up
// before it is pinned.
//
// Each commit is built from what was logged at landing: its child, its
// new pin, when it moved, and the commit the change made in the parent if
// it made one. So two roll-ups racing build the same commits, one wins the
// swap, and the other finds the head already there.

import type { Repos, TreeEntry } from "./artifacts";
import { type GitObject, commitObject, treeObject } from "./objects";
import { type HeadMove, parentOf } from "./project";
import { landingRef } from "./refs";

/** What rolling up needs of `ProjectDO`. */
export interface RollupStore {
	head(branch: string, tracker: string): Promise<string | null>;
	/** Every tracker with a head on `branch`. */
	trackers(branch: string): Promise<string[]>;
	pending(branch: string, parent: string): Promise<HeadMove[]>;
	rolledUp(branch: string, parent: string, old: string, next: string, through: number): Promise<boolean>;
	/** Whether `commit` has ever been `tracker`'s head on `branch`. */
	wasHead(branch: string, tracker: string, commit: string): Promise<boolean>;
}

/** Who a roll-up commit is by. */
export const ROLLUP_SIGNATURE = { name: "newgit", email: "newgit@newgit.dev" };

/** Where a tracker's pins sit in its tree. */
const PINS_PATH = [".newgit", "pins"];

/** How many times a roll-up starts again when the parent's head moved under it. */
const ATTEMPTS = 3;

/** The name a tracker's pin has in its parent's tree. */
export function leafOf(id: string): string {
	const at = id.lastIndexOf("__");
	return at < 0 ? id : id.slice(at + 2);
}

/** A tracker ID as a path, for people: `t3code__repos` is `t3code/repos`. */
function pathOf(id: string): string {
	return id.split("__").join("/");
}

export interface RolledUp {
	/** The parent's head now. */
	head: string | null;
	/** How many commits this roll-up wrote, children's included. */
	written: number;
}

/**
 * Roll `parent` up on `branch`: its pending children first, then it. Its
 * head afterwards. Throws when it cannot: the caller serves the last commit
 * and says it is behind.
 */
export async function rollUp(
	repos: Repos,
	store: RollupStore,
	repoOf: (tracker: string) => string,
	branch: string,
	parent: string,
): Promise<RolledUp> {
	let written = 0;
	const trackers = await store.trackers(branch);
	for (const child of trackers.filter((tracker) => parentOf(tracker) === parent))
		if (trackers.some((tracker) => parentOf(tracker) === child))
			written += (await rollUp(repos, store, repoOf, branch, child)).written;

	for (let attempt = 0; attempt < ATTEMPTS; attempt++) {
		const head = await store.head(branch, parent);
		const moves = await store.pending(branch, parent);
		if (head === null || moves.length === 0) return { head, written };
		const made = await commitsFor(repos, repoOf(parent), head, moves, (commit) =>
			store.wasHead(branch, parent, commit),
		);
		const through = moves.at(-1)!.seq;
		if (made.head === head) {
			// Every pin already says it: nothing to write, only to record.
			if (await store.rolledUp(branch, parent, head, head, through)) return { head, written };
			continue;
		}
		// Objects first, at a landing ref no one reads; then the head moves.
		// Only fast-forwards to changes' own commits: those are there already.
		if (made.objects.length > 0) {
			const sent = await repos.updateRefs(
				repoOf(parent),
				[{ ref: landingRef(made.head), old: null, new: made.head }],
				made.objects,
			);
			if (!sent[landingRef(made.head)] && !(await landed(repos, repoOf(parent), made.head)))
				throw new Error(`${repoOf(parent)} did not take the roll-up's commit ${made.head}`);
		} else if (!(await landed(repos, repoOf(parent), made.head))) {
			throw new Error(`${repoOf(parent)} has no commit ${made.head} to fast-forward to`);
		}
		const won = await store.rolledUp(branch, parent, head, made.head, through);
		// Landing refs nothing will make a head: the changes' commits kept as
		// a merge's second parent, and ours when another roll-up won.
		const spent = [...made.merged, ...(won ? [] : [made.head])];
		if (spent.length > 0)
			await repos
				.updateRefs(
					repoOf(parent),
					spent.map((commit) => ({ ref: landingRef(commit), old: commit, new: null })),
				)
				.catch(() => ({}));
		if (won) return { head: made.head, written: written + made.written };
	}
	throw new Error(`\`${pathOf(parent)}\`'s head kept moving while it rolled up`);
}

/** Whether the repo has `commit` already: a landing ref from a run before this one. */
async function landed(repos: Repos, name: string, commit: string): Promise<boolean> {
	return (await repos.readCommit(name, commit)) !== null;
}

/**
 * The commits that write `moves` down on top of `head`, one per move, in
 * order. A move whose change made its own commit in the parent keeps it:
 * fast-forwarded to when it sits on the head and pins exactly that,
 * otherwise merged with the head, as parallel instances' work is today.
 */
async function commitsFor(
	repos: Repos,
	name: string,
	head: string,
	moves: HeadMove[],
	wasHead: (commit: string) => Promise<boolean>,
): Promise<{ head: string; objects: GitObject[]; written: number; merged: string[] }> {
	const objects: GitObject[] = [];
	const merged: string[] = [];
	let current = head;
	let tree = await commitTree(repos, name, head);
	let written = 0;
	for (const move of moves) {
		const leaf = leafOf(move.tracker);
		const pinned = await replacePin(repos, name, tree, leaf, move.new!);
		if (pinned.tree === tree) continue;
		const via = await keptCommit(repos, name, move, leaf, wasHead);
		if (via !== null && via.parents[0] === current && via.tree === pinned.tree) {
			// Its trees came with it: nothing to send.
			current = via.hash;
			tree = pinned.tree;
			continue;
		}
		objects.push(...pinned.objects);
		const at = Math.floor(Date.parse(move.movedAt) / 1000);
		const commit = await commitObject({
			tree: pinned.tree,
			parents: via === null ? [current] : [current, via.hash],
			author: { ...ROLLUP_SIGNATURE, at },
			committer: { ...ROLLUP_SIGNATURE, at },
			message: `Roll up \`${pathOf(move.tracker)}\` to ${move.new!.slice(0, 12)}`,
		});
		objects.push(commit);
		if (via !== null) merged.push(via.hash);
		current = commit.hash;
		tree = pinned.tree;
		written++;
	}
	return { head: current, objects: dedupe(objects), written, merged };
}

/**
 * The commit a change made in the parent, when it may join the parent's
 * history: one parent, a commit the parent's head really was, and a tree
 * that is exactly that commit's with this one pin moved. So it brings no
 * history and no content the change's approval did not cover. Anything
 * else is left out, and the roll-up writes its own commit instead.
 */
async function keptCommit(
	repos: Repos,
	name: string,
	move: HeadMove,
	leaf: string,
	wasHead: (commit: string) => Promise<boolean>,
) {
	if (move.via === null) return null;
	const via = await repos.readCommit(name, move.via).catch(() => null);
	if (via === null || via.parents.length !== 1 || !(await wasHead(via.parents[0]))) return null;
	const base = await commitTree(repos, name, via.parents[0]).catch(() => null);
	if (base === null) return null;
	const expected = await replacePin(repos, name, base, leaf, move.new!).catch(() => null);
	return expected?.tree === via.tree ? via : null;
}

function dedupe(objects: GitObject[]): GitObject[] {
	return [...new Map(objects.map((object) => [object.hash, object])).values()];
}

/** The tree commit `hash` holds. Read objects stay read: Artifacts' are immutable. */
async function commitTree(repos: Repos, name: string, hash: string): Promise<string> {
	const commit = await repos.readCommit(name, hash);
	if (commit === null) throw new Error(`${name} has no commit ${hash}`);
	return commit.tree;
}

/** Trees made here this run, so a later move reads its own work. */
const madeTrees = new Map<string, TreeEntry[]>();

async function readTree(repos: Repos, name: string, hash: string): Promise<TreeEntry[]> {
	const made = madeTrees.get(hash);
	if (made !== undefined) return made;
	const entries = await repos.readTree(name, hash);
	if (entries === null) throw new Error(`${name} has no tree ${hash}`);
	return entries;
}

/**
 * `tree` with pin `leaf` at `.newgit/pins/` set to `commit`: the new trees
 * on the way down, and the new root tree. The same tree when it pins that
 * already.
 */
async function replacePin(
	repos: Repos,
	name: string,
	tree: string,
	leaf: string,
	commit: string,
): Promise<{ tree: string; objects: GitObject[] }> {
	const levels: TreeEntry[][] = [];
	let at = tree;
	for (const part of PINS_PATH) {
		const entries = await readTree(repos, name, at);
		levels.push(entries);
		const next = entries.find((entry) => entry.name === part && entry.type === "tree");
		if (next === undefined) throw new Error(`${name}'s tree ${tree} has no \`${PINS_PATH.join("/")}\``);
		at = next.hash;
	}
	const pins = await readTree(repos, name, at);
	if (pins.some((entry) => entry.name === leaf && entry.type === "gitlink" && entry.hash === commit))
		return { tree, objects: [] };
	const objects: GitObject[] = [];
	let made = await treeObject([
		...pins.filter((entry) => entry.name !== leaf),
		{ name: leaf, hash: commit, type: "gitlink" },
	]);
	remember(made, [...pins.filter((entry) => entry.name !== leaf), { name: leaf, hash: commit, type: "gitlink" }]);
	objects.push(made);
	for (let depth = PINS_PATH.length - 1; depth >= 0; depth--) {
		const entries = levels[depth].map((entry) =>
			entry.name === PINS_PATH[depth] && entry.type === "tree" ? { ...entry, hash: made.hash } : entry,
		);
		made = await treeObject(entries);
		remember(made, entries);
		objects.push(made);
	}
	return { tree: made.hash, objects };
}

function remember(object: GitObject, entries: TreeEntry[]) {
	if (madeTrees.size > 1000) madeTrees.clear();
	madeTrees.set(object.hash, entries);
}
