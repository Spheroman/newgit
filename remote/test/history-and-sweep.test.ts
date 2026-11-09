import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { commitObject, treeObject } from "../src/objects";
import { landingRef } from "../src/refs";
import { nativeIndex } from "../src/walk";
import { FakeRepos, noEndpoint } from "./fake-repos";
import { sweep } from "../src/sweep";
import { runInDurableObject } from "cloudflare:test";
import type { ProjectDO } from "../src/project";

// Anchoring build step 5 (docs/design/anchoring.md): a parent records each
// child as `history` says, every change (`commits`) or where it is now
// (`pins`), and the sweep rolls up what nobody read and clears the landing
// refs merges leave. New tests live in their own file per AGENTS.md's
// "Working in parallel".

const blob = "45b983be36b73c0788dc9cbcb76cbb80fc7bb057";
const other = "8baef1b4abc478178b004d62031cf7fe6db6f903";

let repos: FakeRepos;
let slug: string;
let jack: string;
let ana: string;
const pending: Promise<unknown>[] = [];
const repo = (tracker: string) => `jack.${slug}.t.${tracker}`;
const path = (rest: string) => `/v1/projects/jack/${slug}${rest}`;

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: (promise) => void pending.push(promise), endpoint: noEndpoint },
	);
	const answer = { status: response.status, body: (await response.json()) as any };
	await Promise.all(pending.splice(0));
	return answer;
}

let clock = 1_790_000_000;
/** A tracker's commit: `file` at its root, and `pins` at `.newgit/pins/`. */
async function commitIn(tracker: string, pins: Record<string, string>, parents: string[], file = blob) {
	const pinsTree = await treeObject(Object.entries(pins).map(([name, hash]) => ({ name, hash, type: "gitlink" as const })));
	const newgit = await treeObject([{ name: "pins", hash: pinsTree.hash, type: "tree" }]);
	const tree = await treeObject([
		{ name: "file.txt", hash: file, type: "blob" },
		{ name: ".newgit", hash: newgit.hash, type: "tree" },
	]);
	const at = clock++;
	const commit = await commitObject({
		tree: tree.hash,
		parents,
		author: { name: "jack", email: "jack@example.com", at },
		committer: { name: "jack", email: "jack@example.com", at },
		message: `work in ${tracker}`,
	});
	await repos.updateRefs(repo(tracker), [], [pinsTree, newgit, tree, commit]);
	return commit.hash;
}

let main: Record<string, string>;

beforeEach(async () => {
	repos = new FakeRepos();
	slug = `anchor-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name: slug, trackers: ["root", "access", "t3code", "site"] });
	await call(jack, "POST", path("/members"), { login: "ana" });
	// Root is the owner's to maintain; the children are anyone's in the project.
	repos.write(repo("access"), "root.toml", 'audience = "project-devs"\ngit = true\nmaintainers = "owner"\n');
	repos.write(repo("access"), "t3code.toml", 'audience = "project-devs"\ngit = true\nmount = "apps/t3code"\n');
	repos.write(repo("access"), "site.toml", 'audience = "project-devs"\ngit = true\nmount = "site"\n');
	const t3code = await commitIn("t3code", {}, []);
	const site = await commitIn("site", {}, []);
	const access = await commitIn("access", {}, []);
	const root = await commitIn("root", { t3code, site, access }, []);
	main = { root, t3code, site, access };
	expect((await call(jack, "PUT", path("/branches/main"), { list: main })).status).toBe(201);
	for (const [tracker, commit] of Object.entries(main)) repos.setRef(repo(tracker), "refs/heads/main", commit);
});

const deps = () => ({ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint });
const id = () => ({ owner: "jack", name: slug });
const project = () => env.PROJECTS.getByName(`jack/${slug}`);

/** Move `tracker`'s head below root, as a landing at it does: root left to roll up. */
async function moveBelowRoot(tracker: string, file: string) {
	const before = (await project().branch("main"))!.list[tracker];
	const next = await commitIn(tracker, {}, [before], file);
	expect(await project().moveHead("main", tracker, before, next, "jack")).toBe(true);
	return next;
}

const rootHead = async () => (await project().branch("main"))!.list.root;

describe("a child recorded as `pins`", () => {
	it("rolls up as one snapshot of where it is now, however many times it moved", async () => {
		repos.write(repo("access"), "site.toml", 'audience = "project-devs"\ngit = true\nmount = "site"\nhistory = "pins"\n');
		const before = await rootHead();
		await moveBelowRoot("site", other);
		const last = await moveBelowRoot("site", blob.replace("4", "7"));
		expect(await sweep(env, deps(), id())).toBe(false);
		const head = await repos.readCommit(repo("root"), await rootHead());
		expect(head?.parents, "one commit, on the head").toEqual([before]);
		const now = (await project().branch("main"))!.list;
		expect(now.site).toBe(last);
		expect(await nativeIndex(repos, (tracker) => repo(tracker), now)).toMatchObject({ matches: true });
	});

	it("beside a `commits` child: one commit per change for it, one snapshot for the pins", async () => {
		repos.write(repo("access"), "site.toml", 'audience = "project-devs"\ngit = true\nmount = "site"\nhistory = "pins"\n');
		const before = await rootHead();
		await moveBelowRoot("t3code", other);
		await moveBelowRoot("site", other);
		await moveBelowRoot("t3code", blob.replace("4", "7"));
		await moveBelowRoot("site", blob.replace("4", "7"));
		await sweep(env, deps(), id());
		// t3code twice, then the pins once: three commits above the head.
		let commit = await repos.readCommit(repo("root"), await rootHead());
		const chain = [];
		while (commit !== null && commit.hash !== before) {
			chain.push(commit.hash);
			commit = await repos.readCommit(repo("root"), commit.parents[0]);
		}
		expect(chain).toHaveLength(3);
		expect(await nativeIndex(repos, (tracker) => repo(tracker), (await project().branch("main"))!.list)).toMatchObject({
			matches: true,
		});
	});
});

describe("the sweep", () => {
	it("rolls up what nobody read", async () => {
		const before = await rootHead();
		await moveBelowRoot("t3code", other);
		expect(await project().hasPending("main")).toBe(true);
		await sweep(env, deps(), id());
		expect(await project().hasPending("main")).toBe(false);
		expect(await rootHead()).not.toBe(before);
	});

	it("clears a spent landing ref at once, and a stray one only once no merge can need it", async () => {
		const landed = await moveBelowRoot("t3code", other);
		repos.setRef(repo("t3code"), landingRef(landed), landed);
		const stray = await commitIn("t3code", {}, [main.t3code], blob.replace("4", "8"));
		repos.setRef(repo("t3code"), landingRef(stray), stray);
		const landings = async () =>
			Object.keys(await repos.refs(repo("t3code"))).filter((ref) => ref.startsWith("refs/newgit/landing/"));

		expect(await sweep(env, deps(), id()), "the stray is left for later").toBe(true);
		expect(await landings()).toEqual([landingRef(stray)]);
		expect(await sweep(env, deps(), id())).toBe(true);
		expect(await landings()).toEqual([landingRef(stray)]);
		expect(await sweep(env, deps(), id())).toBe(false);
		expect(await landings()).toEqual([]);
	});

	it("is set to run after a landing", async () => {
		await project().sweepSoon();
		await runInDurableObject(project(), async (_instance: ProjectDO, state) => {
			expect(await state.storage.getAlarm()).not.toBeNull();
		});
	});
});
