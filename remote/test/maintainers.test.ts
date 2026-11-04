import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

let repos: FakeRepos;
let name: string;
let jack: string;
let ana: string;

const sha = (c: string) => c.repeat(40);
const main = { root: sha("a"), access: sha("b"), web: sha("d"), admin: sha("e") };

beforeEach(async () => {
	repos = new FakeRepos();
	name = `web-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "web", "admin"] });
	await call(jack, "PUT", path("/branches/main"), { list: main });
	await call(jack, "POST", path("/members"), { login: "ana" });
});

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;
// Every holder here is mounted at `.newgit/trackers`, so a definition is
// `<tracker>.toml` at the root of its repo.
const define = (holder: string, tracker: string, text: string) =>
	repos.write(`jack.${name}.t.${holder}`, `${tracker}.toml`, text);

async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as any };
}

/** Merge moving root and `trackers`, each off its own `main` (so its maintainers' to move). */
const begin = (token: string, trackers: string[], off_main = trackers) =>
	call(token, "POST", path("/branches/main/merge"), { base: main, trackers: ["root", ...trackers], off_main });

describe("maintainers", () => {
	it("decide who merges a tracker — the owner too", async () => {
		define("access", "web", 'audience = "project-devs"\ngit = true\nmaintainers = "user:ana"\n');
		const owner = await begin(jack, ["web"]);
		expect(owner.status).toBe(403);
		expect(owner.body.error).toContain("only ana maintains `web`");
		expect((await begin(ana, ["web"])).status).toBe(200);
	});

	it("are not needed to move a pin to a commit already on the tracker's main", async () => {
		define("access", "web", 'audience = "project-devs"\ngit = true\nmaintainers = "user:ana"\n');
		// jack maintains root (project-devs) but not web: moving web's pin
		// along its own `main` changes only root.
		const before = repos.minted.length;
		const along = await begin(jack, ["web"], []);
		expect(along.status).toBe(200);
		// Write credentials for root alone: web's `main` is not this merge's to push.
		expect(along.body.credentials.map((c: { tracker: string }) => c.tracker)).toEqual(["root"]);
		expect(repos.minted.slice(before).filter((m) => m.scope === "write").map((m) => m.name)).toEqual([`jack.${name}.t.root`]);
	});

	it("are handed write credentials for root and the trackers off their main, nothing else", async () => {
		define("access", "web", 'audience = "project-devs"\ngit = true\n');
		define("access", "admin", 'audience = "project-devs"\ngit = true\n');
		const started = await begin(ana, ["web", "admin"], ["admin"]);
		expect(started.status).toBe(200);
		expect(started.body.credentials.map((c: { tracker: string }) => c.tracker)).toEqual(["root", "admin"]);
	});

	it("refuse a merge that does not move root, or names off_main outside trackers", async () => {
		define("access", "web", 'audience = "project-devs"\ngit = true\n');
		const noRoot = await call(ana, "POST", path("/branches/main/merge"), { base: main, trackers: ["web"] });
		expect(noRoot.status).toBe(400);
		expect(noRoot.body.error).toContain("`root`");
		const stray = await call(ana, "POST", path("/branches/main/merge"), {
			base: main,
			trackers: ["root"],
			off_main: ["web"],
		});
		expect(stray.status).toBe(400);
	});

	it("default to the audience: anyone who may read a tracker may change it", async () => {
		define("access", "web", 'audience = "project-devs"\ngit = true\n');
		expect((await begin(ana, ["web"])).status).toBe(200);
	});

	it("make `access` — who may read what — the owner's unless it says otherwise", async () => {
		const member = await begin(ana, ["access"]);
		expect(member.status).toBe(403);
		expect(member.body.error).toContain("project's owner");
		expect((await begin(jack, ["access"])).status).toBe(200);
	});

	it("layer: a tracker owning another's definition governs it", async () => {
		// `admin` (maintained by ana) owns web's and docs's definitions; `access`
		// (the owner's) holds admin's own.
		define(
			"access",
			"admin",
			'audience = "project-devs"\ngit = true\nmaintainers = "user:ana"\nmount = ".newgit/trackers"\npaths = ["web.toml", "docs.toml"]\n',
		);
		define("admin", "web", 'audience = "project-devs"\ngit = true\nmaintainers = "project-devs"\n');
		// A stale copy in `access` is not what counts: admin owns the file now.
		define("access", "web", 'audience = "user:jack"\ngit = true\n');
		define("admin", "docs", 'audience = "project-devs"\ngit = true\n');

		// web's definition is read from admin: ana, a member, may read and merge it.
		expect((await begin(ana, ["web"])).status).toBe(200);
		await call(ana, "PUT", path("/branches/main"), { base: main, list: main });

		// Changing admin itself is ana's; the owner is not admin's maintainer.
		expect((await begin(jack, ["admin"])).status).toBe(403);

		// Adding docs — defined in admin — is admin's maintainers', not the owner's.
		const byOwner = await call(jack, "POST", path("/trackers"), { name: "docs" });
		expect(byOwner.status).toBe(403);
		expect((await call(ana, "POST", path("/trackers"), { name: "docs" })).status).toBe(201);
	});

	it("layer only by the ownership rules: a claim from a shallower mount does not reach into access", async () => {
		// `admin` mounted at `.` claims the view path of web's definition, but
		// `access` is mounted deeper, at `.newgit/trackers`: access decides.
		define(
			"access",
			"admin",
			'audience = "project-devs"\ngit = true\nmaintainers = "user:ana"\npaths = [".newgit/trackers/web.toml"]\n',
		);
		repos.write(`jack.${name}.t.admin`, ".newgit/trackers/web.toml", 'audience = "project-devs"\ngit = true\n');
		define("access", "web", 'audience = "project-devs"\ngit = true\nmaintainers = "user:jack"\n');
		expect((await begin(ana, ["web"])).status).toBe(403);
		expect((await begin(jack, ["web"])).status).toBe(200);
	});
});
