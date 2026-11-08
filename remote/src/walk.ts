// A merge's checks, answered in the Worker from Artifacts' own reads.
//
// Both checks used to run Git in the endpoint's container: start it (or
// restore it), fetch each tracker's history, and answer, while the client
// asked again every ten seconds. Neither needs a history. An index is each
// tracker commit's pins, which always sit at `.newgit/pins/<name>`: three
// tree reads per commit. Where a pin moves is a walk along parents, usually a
// few commits. So the Worker answers both from `readCommit` and `readTree`,
// in milliseconds, and hands a check to the container only when it cannot
// tell: a commit it cannot read, or a walk past `WALK_LIMIT`.
// (Artifacts' binding has no write side and no `merge-base`; it reads
// commits and trees by hash, which is all these need.)

import type { CommitFacts, Repos } from "./artifacts";
import type { AncestryAnswer, IndexAnswer } from "./endpoint";

/** The tracker ID of `name` mounted in `parent` (`crates/newgit-core/src/tracker.rs`'s `child_id`). */
export function childId(parent: string, name: string): string {
	return parent === "root" ? name : `${parent}__${name}`;
}

/** Past this many commits read, a walk gives the question to the container. */
export const WALK_LIMIT = 200;

/**
 * Commits are dated by their committers' clocks, which can be wrong; a walk
 * stops below its target's date by this much, as Git's own does.
 */
const CLOCK_SKEW_SECONDS = 24 * 60 * 60;

/** Not answerable here: the container's Git answers instead. */
export const ASK_GIT = Symbol("ask Git");

/**
 * The index of root commit `list.root`, every pin all the way down, as the
 * container's `newgit endpoint-index` computes it. `ASK_GIT` when any commit
 * on the way cannot be read.
 */
export async function nativeIndex(
	repos: Repos,
	repoOf: (tracker: string) => string,
	list: Record<string, string>,
): Promise<IndexAnswer | typeof ASK_GIT> {
	return orAskGit("index", () => readIndex(repos, repoOf, list));
}

/**
 * Anything Artifacts throws makes the question Git's: a read here is only
 * ever a faster way to the same answer.
 */
async function orAskGit<T>(what: string, read: () => Promise<T | typeof ASK_GIT>): Promise<T | typeof ASK_GIT> {
	try {
		return await read();
	} catch (error) {
		console.error(JSON.stringify({ walk: what, askingGit: error instanceof Error ? error.message : String(error) }));
		return ASK_GIT;
	}
}

async function readIndex(
	repos: Repos,
	repoOf: (tracker: string) => string,
	list: Record<string, string>,
): Promise<IndexAnswer | typeof ASK_GIT> {
	const root = list.root;
	if (root === undefined) return { matches: false, actual: {} };
	const actual: Record<string, string> = {};
	const stack: [string, string][] = [["root", root]];
	while (stack.length > 0) {
		const [tracker, commit] = stack.pop()!;
		if (tracker in actual) continue;
		actual[tracker] = commit;
		const found = await repos.readCommit(repoOf(tracker), commit);
		if (found === null) return ASK_GIT;
		for (const [name, pin] of Object.entries(await pinsIn(repos, repoOf(tracker), found.tree)))
			stack.push([childId(tracker, name), pin]);
	}
	const keys = Object.keys(actual);
	const matches =
		keys.length === Object.keys(list).length && keys.every((tracker) => list[tracker] === actual[tracker]);
	return { matches, actual };
}

/** The gitlinks at `.newgit/pins/` in `tree`, by name. */
async function pinsIn(repos: Repos, name: string, tree: string): Promise<Record<string, string>> {
	let at = tree;
	for (const part of [".newgit", "pins"]) {
		const entries = await repos.readTree(name, at);
		const next = entries?.find((entry) => entry.name === part && entry.type === "tree");
		if (next === undefined) return {};
		at = next.hash;
	}
	const pins: Record<string, string> = {};
	for (const entry of (await repos.readTree(name, at)) ?? []) if (entry.type === "gitlink") pins[entry.name] = entry.hash;
	return pins;
}

/**
 * Per check, whether the new pin descends from the old and whether `main`
 * has it, as `newgit endpoint-ancestry` answers. `ASK_GIT` when a walk runs
 * past its limit.
 */
export async function nativeAncestry(
	repos: Repos,
	repoOf: (tracker: string) => string,
	checks: { tracker: string; old: string; new: string }[],
): Promise<AncestryAnswer[] | typeof ASK_GIT> {
	return orAskGit("ancestry", () => readAncestry(repos, repoOf, checks));
}

async function readAncestry(
	repos: Repos,
	repoOf: (tracker: string) => string,
	checks: { tracker: string; old: string; new: string }[],
): Promise<AncestryAnswer[] | typeof ASK_GIT> {
	const answers: AncestryAnswer[] = [];
	for (const check of checks) {
		const name = repoOf(check.tracker);
		const main = await repos.branchHead(name, "main");
		const onMain = main === null ? false : await isAncestor(repos, name, check.new, main);
		const forward = await isAncestor(repos, name, check.old, check.new);
		if (onMain === ASK_GIT || forward === ASK_GIT) return ASK_GIT;
		answers.push({ tracker: check.tracker, forward, onMain });
	}
	return answers;
}

/**
 * Whether `older` is `newer` or one of its ancestors: a walk back from
 * `newer` along every parent, stopping below `older`'s date. A commit it
 * cannot read is Git's to judge (it may not be visible here yet).
 */
export async function isAncestor(
	repos: Repos,
	name: string,
	older: string,
	newer: string,
): Promise<boolean | typeof ASK_GIT> {
	const target = await repos.readCommit(name, older);
	if (target === null) return ASK_GIT;
	if (older === newer) return true;
	const floor = target.committedAt - CLOCK_SKEW_SECONDS;
	const seen = new Set<string>();
	const frontier = [newer];
	while (frontier.length > 0) {
		const hash = frontier.pop()!;
		if (seen.has(hash)) continue;
		seen.add(hash);
		if (seen.size > WALK_LIMIT) return ASK_GIT;
		const commit: CommitFacts | null = await repos.readCommit(name, hash);
		if (commit === null) return ASK_GIT;
		if (commit.parents.includes(older)) return true;
		// Older than the target by more than any honest clock is off: nothing
		// below it can be the target.
		if (commit.committedAt < floor) continue;
		frontier.push(...commit.parents);
	}
	return false;
}

/**
 * Whether commit `next` changes nothing of `old` but pins: the same tree,
 * `.newgit/pins/` aside. A parent whose commit only moved pins is not touched
 * by the change; it rolls up to it. `ASK_GIT` when a tree cannot be read.
 */
export async function onlyPinsMoved(
	repos: Repos,
	name: string,
	old: string,
	next: string,
): Promise<boolean | typeof ASK_GIT> {
	return orAskGit("pins", async () => {
		const [before, after] = await Promise.all([repos.readCommit(name, old), repos.readCommit(name, next)]);
		if (before === null || after === null) return ASK_GIT;
		return sameBut(repos, name, before.tree, after.tree, [".newgit", "pins"]);
	});
}

/** Whether trees `left` and `right` are the same but for the entry at `skip`. */
async function sameBut(
	repos: Repos,
	name: string,
	left: string,
	right: string,
	skip: string[],
): Promise<boolean | typeof ASK_GIT> {
	if (left === right) return true;
	const [ours, theirs] = await Promise.all([repos.readTree(name, left), repos.readTree(name, right)]);
	if (ours === null || theirs === null) return ASK_GIT;
	const [here, ...below] = skip;
	const byName = (entries: typeof ours) => new Map(entries.map((entry) => [entry.name, entry]));
	const a = byName(ours);
	const b = byName(theirs);
	for (const key of new Set([...a.keys(), ...b.keys()])) {
		if (key === here) continue;
		const x = a.get(key);
		const y = b.get(key);
		if (x?.hash !== y?.hash || x?.type !== y?.type) return false;
	}
	if (here === undefined) return true;
	const x = a.get(here);
	const y = b.get(here);
	if (below.length === 0 || x === undefined || y === undefined || x.type !== "tree" || y.type !== "tree")
		return below.length === 0 || x?.hash === y?.hash;
	return sameBut(repos, name, x.hash, y.hash, below);
}

/**
 * Whether any commit `next` adds to `base` changes anything but pins, each
 * against its first parent. The history a merge brings into a tracker it
 * does not touch must be pins-only too, or the tracker is touched after
 * all: a root file edited and edited back is still root's to approve.
 *
 * "Adds" is what `next` reaches and `base` does not, found as Git finds a
 * merge base: walking back from both at once, newest commit first, marking
 * which side reaches each. A commit's mark is settled once every newer one
 * is walked, so a commit only `next` reaches is checked then; a commit
 * already in history (an older landing's, below the base) is not. A commit
 * `isApproved` says the head has been counts as reached from `base`. Clocks
 * that lie can only make a commit look added, which checks more, never
 * less. A commit with no parent counts as touching. `ASK_GIT` when it
 * cannot be read or runs past `WALK_LIMIT`.
 */
export async function historyTouches(
	repos: Repos,
	name: string,
	base: string,
	next: string,
	isApproved: (commit: string) => Promise<boolean>,
): Promise<boolean | typeof ASK_GIT> {
	return orAskGit("history", async () => {
		const NEXT = 1;
		const BASE = 2;
		const marks = new Map<string, number>();
		const commits = new Map<string, CommitFacts>();
		const queued = new Set<string>();
		const read = async (hash: string) => {
			const known = commits.get(hash);
			if (known !== undefined) return known;
			const found = await repos.readCommit(name, hash);
			if (found !== null) commits.set(hash, found);
			return found;
		};
		const mark = async (hash: string, side: number) => {
			const was = marks.get(hash) ?? 0;
			if (side === NEXT && was === 0 && (await isApproved(hash))) side |= BASE;
			if ((was | side) === was) return;
			marks.set(hash, was | side);
			queued.add(hash);
		};
		await mark(base, BASE);
		await mark(next, NEXT);
		let walked = 0;
		// Walk while anything only `next` reaches is left to settle.
		while ([...queued].some((hash) => marks.get(hash) === NEXT)) {
			if (++walked > WALK_LIMIT) return ASK_GIT;
			let newest: string | undefined;
			let newestAt = -Infinity;
			for (const hash of queued) {
				const commit = await read(hash);
				if (commit === null) return ASK_GIT;
				if (commit.committedAt > newestAt) {
					newest = hash;
					newestAt = commit.committedAt;
				}
			}
			const hash = newest!;
			queued.delete(hash);
			const commit = commits.get(hash)!;
			const side = marks.get(hash)!;
			if (side === NEXT) {
				if (commit.parents.length === 0) return true;
				const only = await onlyPinsMoved(repos, name, commit.parents[0], hash);
				if (only !== true) return only === ASK_GIT ? ASK_GIT : true;
			}
			for (const parent of commit.parents) await mark(parent, side);
		}
		return false;
	});
}
