// Moving a repo's refs from the Worker, with no container and no objects.
//
// A merge sends its commits first, to landing refs no one reads. The list in
// `ProjectDO` moving is the moment it lands; the repos' branches follow, and
// this is how they do. It is Git's own smart HTTP push, of ref updates alone:
// each command names the value the ref must hold now, so every update is a
// compare-and-swap Artifacts makes for us ("stale ref" when it does not
// hold), and an empty pack carries no objects because none are needed.

/** One ref to move: `old` is what it must hold now, `new` what it moves to. Null is absent. */
export interface RefUpdate {
	ref: string;
	old: string | null;
	new: string | null;
}

const ZERO = "0".repeat(40);
const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Where a merge's commit waits in a tracker's repo until the list moves. */
export function landingRef(commit: string): string {
	return `refs/newgit/landing/${commit}`;
}

function pkt(line: string): Uint8Array {
	const body = encoder.encode(line);
	return concat(encoder.encode((body.length + 4).toString(16).padStart(4, "0")), body);
}

function concat(...parts: Uint8Array[]): Uint8Array {
	const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
	let at = 0;
	for (const part of parts) {
		out.set(part, at);
		at += part.length;
	}
	return out;
}

/** A pack of no objects: its header, and the SHA-1 Git checks it by. */
async function emptyPack(): Promise<Uint8Array> {
	const header = new Uint8Array([0x50, 0x41, 0x43, 0x4b, 0, 0, 0, 2, 0, 0, 0, 0]);
	return concat(header, new Uint8Array(await crypto.subtle.digest("SHA-1", header)));
}

/** The body of a `git-receive-pack` request that makes `updates` and sends nothing else. */
export async function receivePackBody(updates: RefUpdate[]): Promise<Uint8Array> {
	const lines = updates.map((update, index) => {
		const line = `${update.old ?? ZERO} ${update.new ?? ZERO} ${update.ref}`;
		return pkt(index === 0 ? `${line}\0report-status\n` : `${line}\n`);
	});
	// Only deletes send no pack at all: Git refuses one then.
	const pack = updates.every((update) => update.new === null) ? new Uint8Array() : await emptyPack();
	return concat(...lines, encoder.encode("0000"), pack);
}

/** Split pkt-lines; a flush ends nothing here, it is only skipped. */
function pktLines(bytes: Uint8Array): string[] {
	const lines: string[] = [];
	let at = 0;
	while (at + 4 <= bytes.length) {
		const length = Number.parseInt(decoder.decode(bytes.subarray(at, at + 4)), 16);
		if (!Number.isFinite(length)) break;
		if (length < 4) {
			at += 4;
			continue;
		}
		lines.push(decoder.decode(bytes.subarray(at + 4, at + length)).replace(/\n$/, ""));
		at += length;
	}
	return lines;
}

/** Every ref a `git-receive-pack` advertisement names, at its commit. */
export function parseAdvertisement(bytes: Uint8Array): Record<string, string> {
	const refs: Record<string, string> = {};
	for (const line of pktLines(bytes)) {
		const match = line.match(/^([0-9a-f]{40}) ([^\0]+)/);
		// An empty repo advertises `capabilities^{}` at the zero id.
		if (match && match[1] !== ZERO && match[2].startsWith("refs/")) refs[match[2]] = match[1];
	}
	return refs;
}

/**
 * Per ref, whether it moved, from a `report-status`. A pack that did not
 * unpack moved nothing.
 */
export function parseReport(bytes: Uint8Array, updates: RefUpdate[]): Record<string, boolean> {
	const lines = pktLines(bytes);
	const unpacked = lines.includes("unpack ok");
	const moved: Record<string, boolean> = {};
	for (const update of updates) moved[update.ref] = unpacked && lines.includes(`ok ${update.ref}`);
	return moved;
}
