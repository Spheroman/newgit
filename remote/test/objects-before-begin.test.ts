import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { commitObject, treeObject } from "../src/objects";
import { landingRef } from "../src/refs";
import { nativeIndex } from "../src/walk";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A merge's commits sent before it begins (`POST /objects/<tracker>/<commit>`):
// the Worker pushes them to landing refs itself, so `begin` reads them,
// works out where the merge lands and who approves it, and hands out no
// write password. New tests live in their own file per AGENTS.md's
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

async function call(token: string, method: string, route: string, body?: unknown, raw?: Uint8Array) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers:
				raw === undefined
					? { "content-type": "application/json", authorization: `Bearer ${token}` }
					: { "content-type": "application/x-git-packfile", authorization: `Bearer ${token}`, "content-length": String(raw.length) },
			body: raw ?? (body === undefined ? undefined : JSON.stringify(body)),
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

/** A pack's bytes: the fake does not read them; the test adds what they hold. */
const pack = new TextEncoder().encode("PACK....");
const upload = (token: string, tracker: string, commit: string) =>
	call(token, "POST", path(`/objects/${tracker}/${commit}`), undefined, pack);

describe("sending a merge's commits before it begins", () => {
	it("lands them at their landing ref", async () => {
		const next = await commitIn("t3code", {}, [main.t3code], other);
		const sent = await upload(ana, "t3code", next);
		expect(sent.status, JSON.stringify(sent.body)).toBe(200);
		expect(sent.body.ref).toBe(landingRef(next));
		expect((await repos.refs(repo("t3code")))[landingRef(next)]).toBe(next);
		expect(repos.packs.at(-1)).toMatchObject({ name: repo("t3code"), bytes: pack.length });
	});

	it("is answered again for a pack sent before, and refused one that does not hold its commit", async () => {
		const next = await commitIn("t3code", {}, [main.t3code], other);
		expect((await upload(jack, "t3code", next)).status).toBe(200);
		expect((await upload(jack, "t3code", next)).status, "a run that stopped sends again").toBe(200);
		const missing = "1".repeat(40);
		const refused = await upload(jack, "t3code", missing);
		expect(refused.status).toBe(400);
		expect(refused.body.error).toContain("does not hold it");
	});

	it("is a person's to send, for a tracker in Git they may read", async () => {
		const agent = (await call(jack, "POST", "/v1/tokens", { name: "bot" })).body.token;
		const next = await commitIn("t3code", {}, [main.t3code], other);
		expect((await upload(agent, "t3code", next)).status).toBe(403);
		expect((await upload(jack, "nope", next)).status).toBe(404);
		repos.write(repo("access"), "secret.toml", 'audience = "user:jack"\ngit = true\nmount = "secret"\n');
		await call(jack, "POST", path("/trackers"), { name: "secret" });
		expect((await upload(ana, "secret", next)).status).toBe(403);
	});

	it("refuses a pack too large for the Worker: that one is pushed with a password", async () => {
		const next = await commitIn("t3code", {}, [main.t3code], other);
		const response = await handle(
			new Request(`https://newgit.dev${path(`/objects/t3code/${next}`)}`, {
				method: "POST",
				headers: { authorization: `Bearer ${jack}`, "content-length": String(200 * 1024 * 1024) },
				body: pack,
			}),
			env,
			{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
		);
		expect(response.status).toBe(413);
	});
});

describe("a merge whose commits were sent first", () => {
	/** A change to `tracker`, its commits sent before `begin`, as the client does. */
	async function sentFirst(tracker: string, token: string) {
		const next = await commitIn(tracker, {}, [main[tracker]], other);
		const pins = Object.fromEntries(Object.entries(main).filter(([name]) => name !== "root"));
		const root = await commitIn("root", { ...pins, [tracker]: next }, [main.root]);
		expect((await upload(token, tracker, next)).status).toBe(200);
		expect((await upload(token, "root", root)).status).toBe(200);
		const list = { ...main, root, [tracker]: next };
		return { list, begun: await call(token, "POST", path("/branches/main/merge"), { base: main, list, uploaded: true }) };
	}

	it("needs only the touched tracker's maintainers, and gets no password", async () => {
		const { list, begun } = await sentFirst("t3code", ana);
		expect(begun.status, JSON.stringify(begun.body)).toBe(200);
		expect(begun.body.anchor, "begin reads the commits now").toBe("t3code");
		expect(begun.body.credentials).toEqual([]);
		expect(begun.body.writes, "what it may move, without a password").toEqual(expect.arrayContaining(["root", "t3code"]));
		const landed = await call(ana, "PUT", path("/branches/main"), { base: main, list });
		expect(landed.status, JSON.stringify(landed.body)).toBe(200);
		expect(landed.body.anchor).toBe("t3code");
		expect((await call(jack, "GET", path("/branches/main"))).body.list.t3code).toBe(list.t3code);
	});

	it("that touches root still asks root's maintainers", async () => {
		const root = await commitIn("root", { t3code: main.t3code, site: main.site, access: main.access }, [main.root], other);
		expect((await upload(ana, "root", root)).status).toBe(200);
		const list = { ...main, root };
		const begun = await call(ana, "POST", path("/branches/main/merge"), { base: main, list, uploaded: true });
		expect(begun.status).toBe(403);
	});
});
