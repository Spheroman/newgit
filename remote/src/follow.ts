// After a landing, the repos' branches follow the list.
//
// The list in `ProjectDO` is the truth, and moving it is the moment a merge
// lands. Root's branch in root's repo, and each child branch a merge asked to
// move, catch up afterwards: from the Worker, by compare-and-swap, with no
// objects to send (they are at landing refs already). Two merges landing one
// after the other may follow in either order, so root's branch follows the
// head *now*, not the one this landing made: whichever runs last leaves it
// right, and the other finds nothing to do.

import type { Repos } from "./artifacts";
import { type RefUpdate, landingRef } from "./refs";

/** A child branch a merge asks to move once it lands (`crates/newgit-core/src/remote.rs`'s `RefMove`). */
export interface RefMove {
	tracker: string;
	branch: string;
	old: string | null;
	new: string;
}

/** What following needs of `ProjectDO`. */
export interface Heads {
	/** `tracker`'s head on `branch` now, or null. */
	head(branch: string, tracker: string): Promise<string | null>;
	/** Whether `commit` has ever been `tracker`'s head on `branch`. */
	wasHead(branch: string, tracker: string, commit: string): Promise<boolean>;
}

/** How many times root's branch is read and moved again when another landing moved it between. */
const FOLLOW_ATTEMPTS = 3;

/**
 * Bring root's `branch` to its head, then make `moves`. Never throws: the
 * landing stands whatever happens here, so what could not follow is said,
 * for the next landing (or anyone) to finish.
 */
export async function followLanding(
	repos: Repos,
	heads: Heads,
	repoOf: (tracker: string) => string,
	branch: string,
	moves: RefMove[],
): Promise<string[]> {
	const behind: string[] = [];
	try {
		const lagging = await followRoot(repos, heads, repoOf("root"), branch);
		if (lagging !== null) behind.push(lagging);
	} catch (error) {
		behind.push(`root's \`${branch}\` did not follow its head yet: ${message(error)}`);
	}
	for (const move of moves) {
		try {
			const name = repoOf(move.tracker);
			const ref = `refs/heads/${move.branch}`;
			const updates: RefUpdate[] = [];
			if (move.old !== move.new) updates.push({ ref, old: move.old, new: move.new });
			updates.push({ ref: landingRef(move.new), old: move.new, new: null });
			const moved = await repos.updateRefs(name, updates);
			if (move.old !== move.new && !moved[ref])
				behind.push(
					`\`${move.tracker}\`'s \`${move.branch}\` moved while this merge ran, so it did not catch up: \`newgit pull ${move.tracker}\` in an instance, and merge that`,
				);
		} catch (error) {
			behind.push(`\`${move.tracker}\`'s \`${move.branch}\` did not catch up: ${message(error)}`);
		}
	}
	return behind;
}

/**
 * Root's `branch` to root's head. A commit there that no list ever named
 * (pushed past the remote, or by a client from before landing refs) is kept
 * at `refs/newgit/stray/<commit>`, never lost, and the branch still follows:
 * the list is the truth. Null when it follows, or what is behind.
 */
async function followRoot(repos: Repos, heads: Heads, name: string, branch: string): Promise<string | null> {
	const ref = `refs/heads/${branch}`;
	for (let attempt = 0; attempt < FOLLOW_ATTEMPTS; attempt++) {
		const head = await heads.head(branch, "root");
		if (head === null) return null;
		const refs = await repos.refs(name);
		const held = refs[ref] ?? null;
		const landing = landingRef(head);
		const updates: RefUpdate[] = [];
		if (held !== head) {
			updates.push({ ref, old: held, new: head });
			if (held !== null && !(await heads.wasHead(branch, "root", held)))
				updates.push({ ref: `refs/newgit/stray/${held}`, old: null, new: held });
		}
		if (refs[landing] !== undefined) updates.push({ ref: landing, old: refs[landing], new: null });
		if (updates.length === 0) return null;
		const moved = await repos.updateRefs(name, updates);
		if (held === head || moved[ref]) return null;
	}
	return `root's \`${branch}\` kept moving under this landing; the next one brings it to the list`;
}

function message(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
