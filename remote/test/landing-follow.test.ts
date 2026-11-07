import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { landingRef, parseAdvertisement, parseReport, receivePackBody } from "../src/refs";
import { FakeRepos, noEndpoint } from "./fake-repos";

// Landing without a lock (docs/design/anchoring.md, build order step 3): a
// merge's commits wait at landing refs, the list moves by one
// compare-and-swap, and only then do the repos' branches follow, moved by the
// Worker. New tests live in their own file per AGENTS.md's "Working in parallel".

let repos: FakeRepos;
let name: string;
let jack: string;

const sha = (c: string) => c.repeat(40);
const main = { root: sha("a"), t3code: sha("b") };
const rootRepo = () => `jack.${name}.t.root`;
const t3Repo = () => `jack.${name}.t.t3code`;

/** Whether each child pin moves along its `main`; off it, the merge gets a write password for it. */
let onMain = true;

beforeEach(async () => {
	repos = new FakeRepos();
	onMain = true;
	name = `follow-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "t3code"] });
	const defs = `jack.${name}.t.access`;
	repos.write(defs, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(defs, "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\n');
	await call(jack, "PUT", path("/branches/main"), { list: main });
	repos.setRef(rootRepo(), "refs/heads/main", main.root);
	repos.setRef(t3Repo(), "refs/heads/main", main.t3code);
});

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;

const ancestry = async (_project: string, plan: { checks: { tracker: string }[] }) =>
	plan.checks.map((check) => ({ tracker: check.tracker, forward: true, onMain }));
const index = async (_project: string, plan: { list: Record<string, string> }) => ({ matches: true, actual: plan.list });

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint, ancestry, index },
	);
	return { status: response.status, body: (await response.json()) as any };
}

/** Begin, push to landing refs as the client does, and finish. */
async function merge(list: Record<string, string>, moves: unknown[] = [], base = main) {
	const begun = await call(jack, "POST", path("/branches/main/merge"), { base, list });
	expect(begun.status, JSON.stringify(begun.body)).toBe(200);
	repos.setRef(rootRepo(), landingRef(list.root), list.root);
	for (const move of moves as { tracker: string; new: string }[])
		repos.setRef(`jack.${name}.t.${move.tracker}`, landingRef(move.new), move.new);
	return call(jack, "PUT", path("/branches/main"), { base, list, moves });
}

const refsOf = async (repo: string) => repos.refs(repo);

describe("a landing", () => {
	it("moves root's branch after the list, and clears its landing ref", async () => {
		const moved = { ...main, root: sha("c") };
		const landed = await merge(moved);
		expect(landed.status).toBe(200);
		expect(landed.body.behind).toEqual([]);
		expect(landed.body.list).toEqual(moved);
		const refs = await refsOf(rootRepo());
		expect(refs["refs/heads/main"]).toBe(sha("c"));
		expect(refs[landingRef(sha("c"))]).toBeUndefined();
		// By compare-and-swap on what it held.
		const update = repos.updates.find((made) => made.update.ref === "refs/heads/main");
		expect(update?.update).toEqual({ ref: "refs/heads/main", old: sha("a"), new: sha("c") });
	});

	it("makes a child's catch-up the merge asks for, and only one it was handed", async () => {
		onMain = false;
		const moved = { root: sha("c"), t3code: sha("d") };
		const asked = [{ tracker: "t3code", branch: "main", old: sha("b"), new: sha("d") }];
		const landed = await merge(moved, asked);
		expect(landed.status, JSON.stringify(landed.body)).toBe(200);
		expect((await refsOf(t3Repo()))["refs/heads/main"]).toBe(sha("d"));
		expect((await refsOf(t3Repo()))[landingRef(sha("d"))]).toBeUndefined();
	});

	it("refuses to move a tracker the merge was not handed", async () => {
		const moved = { ...main, root: sha("c") };
		const refused = await merge(moved, [{ tracker: "t3code", branch: "main", old: sha("b"), new: sha("e") }]);
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("not handed `t3code`");
		expect((await call(jack, "GET", path("/branches/main"))).body.list, "nothing landed").toEqual(main);
	});

	it("stands when a child's branch moved meanwhile, and says it did not catch up", async () => {
		onMain = false;
		const moved = { root: sha("c"), t3code: sha("d") };
		repos.setRef(t3Repo(), "refs/heads/main", sha("9"));
		const landed = await merge(moved, [{ tracker: "t3code", branch: "main", old: sha("b"), new: sha("d") }]);
		expect(landed.status).toBe(200);
		expect(landed.body.list).toEqual(moved);
		expect(landed.body.behind.join("\n")).toContain("`t3code`'s `main` moved while this merge ran");
		expect((await refsOf(t3Repo()))["refs/heads/main"], "left as it was").toBe(sha("9"));
	});

	it("brings a lagging root branch to the head now, from wherever an older head left it", async () => {
		const first = { ...main, root: sha("c") };
		expect((await merge(first)).status).toBe(200);
		// As if the first landing's follow had not run yet.
		repos.setRef(rootRepo(), "refs/heads/main", main.root);
		const second = { ...main, root: sha("d") };
		const landed = await merge(second, [], first);
		expect(landed.status).toBe(200);
		expect((await refsOf(rootRepo()))["refs/heads/main"]).toBe(sha("d"));
		expect(Object.keys(await refsOf(rootRepo())).some((ref) => ref.startsWith("refs/newgit/stray/"))).toBe(false);
	});

	it("keeps a commit no list named at a stray ref, and still follows the list", async () => {
		repos.setRef(rootRepo(), "refs/heads/main", sha("7"));
		const moved = { ...main, root: sha("c") };
		expect((await merge(moved)).status).toBe(200);
		const refs = await refsOf(rootRepo());
		expect(refs["refs/heads/main"]).toBe(sha("c"));
		expect(refs[`refs/newgit/stray/${sha("7")}`]).toBe(sha("7"));
	});

	it("finished again after it landed (a run that never heard), follows again", async () => {
		onMain = false;
		const moved = { root: sha("c"), t3code: sha("d") };
		const asked = [{ tracker: "t3code", branch: "main", old: sha("b"), new: sha("d") }];
		expect((await merge(moved, asked)).status).toBe(200);
		// As if the request had dropped before anything followed.
		repos.setRef(rootRepo(), "refs/heads/main", main.root);
		repos.setRef(t3Repo(), "refs/heads/main", main.t3code);
		const again = await call(jack, "PUT", path("/branches/main"), { base: main, list: moved, moves: asked });
		expect(again.status, JSON.stringify(again.body)).toBe(200);
		expect(again.body.list).toEqual(moved);
		expect((await refsOf(rootRepo()))["refs/heads/main"]).toBe(sha("c"));
		expect((await refsOf(t3Repo()))["refs/heads/main"], "the catch-up it was allowed").toBe(sha("d"));
	});

	it("clears the landing ref an earlier landing left when it followed late", async () => {
		const first = { ...main, root: sha("c") };
		expect((await merge(first)).status).toBe(200);
		// The first landing's follow had not cleared its own yet.
		repos.setRef(rootRepo(), landingRef(sha("c")), sha("c"));
		const second = { ...main, root: sha("d") };
		expect((await merge(second, [], first)).status).toBe(200);
		const refs = await refsOf(rootRepo());
		expect(Object.keys(refs).filter((ref) => ref.startsWith("refs/newgit/landing/"))).toEqual([]);
	});
});

describe("a ref-only push", () => {
	const decode = (bytes: Uint8Array) => new TextDecoder().decode(bytes);

	it("names each update, asks for a report, and sends an empty pack", async () => {
		const body = await receivePackBody([
			{ ref: "refs/heads/main", old: sha("a"), new: sha("b") },
			{ ref: landingRef(sha("b")), old: sha("b"), new: null },
		]);
		const text = decode(body);
		const first = `${sha("a")} ${sha("b")} refs/heads/main\0report-status\n`;
		expect(text.startsWith((first.length + 4).toString(16).padStart(4, "0") + first)).toBe(true);
		expect(text).toContain(`${sha("b")} ${"0".repeat(40)} ${landingRef(sha("b"))}\n0000PACK`);
		// Header (12 bytes) and its SHA-1 (20): a pack of nothing.
		const pack = body.subarray(text.indexOf("0000PACK") + 4);
		expect(pack.length).toBe(32);
		expect([...pack.subarray(4, 12)]).toEqual([0, 0, 0, 2, 0, 0, 0, 0]);
	});

	it("sends no pack when it only deletes", async () => {
		const text = decode(await receivePackBody([{ ref: "refs/x", old: sha("a"), new: null }]));
		expect(text.endsWith("0000")).toBe(true);
	});

	it("reads an advertisement, and a report of what moved", () => {
		const pkt = (line: string) => (line.length + 4).toString(16).padStart(4, "0") + line;
		const advertised = new TextEncoder().encode(
			pkt(`${sha("a")} refs/heads/main\0report-status delete-refs\n`) + pkt(`${sha("b")} ${landingRef(sha("b"))}\n`) + "0000",
		);
		expect(parseAdvertisement(advertised)).toEqual({ "refs/heads/main": sha("a"), [landingRef(sha("b"))]: sha("b") });
		const empty = new TextEncoder().encode(pkt(`${"0".repeat(40)} capabilities^{}\0report-status\n`) + "0000");
		expect(parseAdvertisement(empty)).toEqual({});
		// As Artifacts answered when probed: one held, one stale.
		const report = new TextEncoder().encode(
			pkt("unpack ok\n") + pkt("ok refs/heads/main\n") + pkt("ng refs/x stale ref\n") + "0000",
		);
		const updates = [
			{ ref: "refs/heads/main", old: sha("a"), new: sha("b") },
			{ ref: "refs/x", old: null, new: sha("b") },
		];
		expect(parseReport(report, updates)).toEqual({ "refs/heads/main": true, "refs/x": false });
	});
});
