// Git objects, made in the Worker.
//
// Artifacts' binding reads objects and cannot write them, and its Git
// endpoint takes a push. A roll-up needs a handful of new objects: one
// commit, and the trees on the path to `.newgit/pins/`. So the Worker
// encodes them exactly as Git does, hashes them, packs them, and pushes
// the pack with the ref-only push it already makes (`refs.ts`). Never a
// reimplementation of Git's object model: the same bytes Git writes, for
// the two object types a roll-up makes, checked against hashes Git made.

import type { TreeEntry } from "./artifacts";

/** An object to send: its type, its body as Git stores it, and its id. */
export interface GitObject {
	type: "commit" | "tree";
	body: Uint8Array;
	hash: string;
}

const encoder = new TextEncoder();

/** Git's mode for each kind of tree entry, as a tree stores it. */
const MODES: Record<TreeEntry["type"], string> = {
	tree: "40000",
	blob: "100644",
	exec: "100755",
	symlink: "120000",
	gitlink: "160000",
};

function concat(parts: Uint8Array[]): Uint8Array {
	const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
	let at = 0;
	for (const part of parts) {
		out.set(part, at);
		at += part.length;
	}
	return out;
}

function hex(bytes: Uint8Array): string {
	return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function unhex(text: string): Uint8Array {
	const out = new Uint8Array(text.length / 2);
	for (let index = 0; index < out.length; index++) out[index] = Number.parseInt(text.slice(index * 2, index * 2 + 2), 16);
	return out;
}

async function sha1(bytes: Uint8Array): Promise<Uint8Array> {
	return new Uint8Array(await crypto.subtle.digest("SHA-1", bytes));
}

/** An object's id: SHA-1 of `<type> <size>\0` and its body. */
export async function hashObject(type: GitObject["type"], body: Uint8Array): Promise<string> {
	return hex(await sha1(concat([encoder.encode(`${type} ${body.length}\0`), body])));
}

/**
 * Git's order for tree entries: by name as bytes, a tree's name compared as
 * if it ended in `/`. (So `a.b` < `a.txt` < the tree `a`.)
 */
function sortKey(entry: TreeEntry): Uint8Array {
	return encoder.encode(entry.type === "tree" ? `${entry.name}/` : entry.name);
}

function compareBytes(left: Uint8Array, right: Uint8Array): number {
	const length = Math.min(left.length, right.length);
	for (let index = 0; index < length; index++) if (left[index] !== right[index]) return left[index] - right[index];
	return left.length - right.length;
}

/** A tree object of `entries`, in Git's order. */
export async function treeObject(entries: TreeEntry[]): Promise<GitObject> {
	const sorted = [...entries].sort((left, right) => compareBytes(sortKey(left), sortKey(right)));
	const body = concat(
		sorted.flatMap((entry) => [encoder.encode(`${MODES[entry.type]} ${entry.name}\0`), unhex(entry.hash)]),
	);
	return { type: "tree", body, hash: await hashObject("tree", body) };
}

/** Who made a commit, and when (Unix seconds, UTC). */
export interface Signature {
	name: string;
	email: string;
	at: number;
}

/** A commit object, as `git commit-tree` writes one. */
export async function commitObject(commit: {
	tree: string;
	parents: string[];
	author: Signature;
	committer: Signature;
	message: string;
}): Promise<GitObject> {
	const sign = (who: Signature) => `${who.name} <${who.email}> ${who.at} +0000`;
	const message = commit.message.endsWith("\n") ? commit.message : `${commit.message}\n`;
	const text = [
		`tree ${commit.tree}`,
		...commit.parents.map((parent) => `parent ${parent}`),
		`author ${sign(commit.author)}`,
		`committer ${sign(commit.committer)}`,
		"",
		message,
	].join("\n");
	const body = encoder.encode(text);
	return { type: "commit", body, hash: await hashObject("commit", body) };
}

const PACK_TYPES: Record<GitObject["type"], number> = { commit: 1, tree: 2 };

async function deflate(bytes: Uint8Array): Promise<Uint8Array> {
	// "deflate" is zlib's format (RFC 1950), which is what a pack holds.
	const stream = new Blob([bytes as Uint8Array<ArrayBuffer>]).stream().pipeThrough(new CompressionStream("deflate"));
	return new Uint8Array(await new Response(stream).arrayBuffer());
}

/** An object's header in a pack: its type, and its size in 7-bit groups. */
function packHeader(type: number, size: number): Uint8Array {
	const bytes = [((size > 15 ? 0x80 : 0) | (type << 4) | (size & 15))];
	let rest = size >> 4;
	while (rest > 0) {
		bytes.push((rest > 127 ? 0x80 : 0) | (rest & 127));
		rest >>= 7;
	}
	return new Uint8Array(bytes);
}

/** A version 2 pack of `objects`, whole (no deltas), with its SHA-1 trailer. */
export async function packObjects(objects: GitObject[]): Promise<Uint8Array> {
	const header = new Uint8Array(12);
	header.set(encoder.encode("PACK"));
	new DataView(header.buffer).setUint32(4, 2);
	new DataView(header.buffer).setUint32(8, objects.length);
	const parts: Uint8Array[] = [header];
	for (const object of objects) {
		parts.push(packHeader(PACK_TYPES[object.type], object.body.length), await deflate(object.body));
	}
	const body = concat(parts);
	return concat([body, await sha1(body)]);
}
