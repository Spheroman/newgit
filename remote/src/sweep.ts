// The sweep (docs/design/anchoring.md, *When a parent catches up*): what
// keeps queues short and repos tidy when nobody reads. A read rolls up what
// it needs; the sweep rolls up the rest, so the first reader of a tree
// nobody read in a month pays nothing, and clears the landing refs that
// merges and roll-ups leave behind. `ProjectDO`'s alarm runs it within the
// hour of a landing or an upload, and again while anything is left.

import { type Deps, projectStub, settle } from "./index";
import { type ProjectId, projectKey, repoName } from "./names";
import { type RefUpdate } from "./refs";

/** How often the sweep runs while there is anything for it. */
export const SWEEP_EVERY_MS = 60 * 60_000;

/**
 * How many sweeps must see a landing ref no head explains before it goes:
 * two hours or more since it was first seen, longer than a merge stays
 * pending (70 minutes), so a merge in flight never loses its commits' ref.
 */
const SIGHTINGS_BEFORE_CLEARING = 3;

/** Run one sweep of project `id`. True when something is left for the next. */
export async function sweep(env: Env, deps: Deps, id: ProjectId): Promise<boolean> {
	const project = projectStub(env, id);
	let left = false;
	for (const branch of await project.pendingBranches())
		if (!(await settle(deps, project, id, branch))) left = true;

	const trackers = new Set((await project.branches()).flatMap((branch) => Object.keys(branch.list)));
	const seen = await project.landingSightings();
	const next: Record<string, Record<string, number>> = {};
	let cleared = 0;
	for (const tracker of trackers) {
		const name = repoName({ kind: "tracker", project: id, tracker });
		const refs = await deps.repos.refs(name).catch(() => null);
		if (refs === null) continue;
		const spent: RefUpdate[] = [];
		for (const [ref, commit] of Object.entries(refs)) {
			if (!ref.startsWith("refs/newgit/landing/")) continue;
			// Landed, or kept as a change's own commit: in history, so spent.
			if (await project.knownCommit(tracker, commit)) {
				spent.push({ ref, old: commit, new: null });
				continue;
			}
			// A merge begun and never landed, or one still in flight: gone only
			// once seen long enough that no merge can still need it.
			const sightings = (seen[name]?.[commit] ?? 0) + 1;
			if (sightings >= SIGHTINGS_BEFORE_CLEARING) spent.push({ ref, old: commit, new: null });
			else {
				next[name] = { ...next[name], [commit]: sightings };
				left = true;
			}
		}
		if (spent.length > 0) {
			const moved = await deps.repos.updateRefs(name, spent).catch(() => ({}) as Record<string, boolean>);
			cleared += Object.values(moved).filter(Boolean).length;
		}
	}
	await project.setLandingSightings(next);
	console.log(JSON.stringify({ sweep: projectKey(id), cleared, waiting: Object.values(next).flatMap(Object.keys).length, left }));
	return left;
}
