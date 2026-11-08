// Where a change lands: its anchor, the lowest tracker covering every one it
// touches (docs/design/anchoring.md, *Landing*). Trackers form a tree by
// their IDs: `t3code__repos` is mounted in `t3code`, `t3code` in `root`.

import { parentOf } from "./project";

/** `id` and every tracker above it, nearest first, ending at `root`. */
export function lineage(id: string): string[] {
	const chain = [id];
	for (let up = parentOf(id); up !== null; up = parentOf(up)) chain.push(up);
	return chain;
}

/** Whether `id` is `anchor` or mounted, however deep, inside it. */
export function within(id: string, anchor: string): boolean {
	return lineage(id).includes(anchor);
}

/** The lowest tracker that is or contains every one of `ids`; `root` for none. */
export function anchorOf(ids: string[]): string {
	if (ids.length === 0) return "root";
	let common = lineage(ids[0]);
	for (const id of ids.slice(1)) {
		const theirs = new Set(lineage(id));
		common = common.filter((tracker) => theirs.has(tracker));
	}
	return common[0] ?? "root";
}
