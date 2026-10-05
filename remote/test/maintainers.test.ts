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
const main = { root: sha("a"), access: sha("b"), project: sha("c"), web: sha("d"), admin: sha("e") };

beforeEach(async () => {
	repos = new FakeRepos();
	name = `web-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "project", "web", "admin"] });
	await call(jack, "PUT", path("/branches/main"), { list: main });
	await call(jack, "POST", path("/members"), { login: "ana" });
});

const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;
const define = (holder: string, tracker: string, text: string) =>
	repos.write(`jack.${name}.t.${holder}`, `.newgit/trackers/${tracker}.toml`, text);

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

const begin = (token: string, trackers: string[]) =>
	call(token, "POST", path("/branches/main/merge"), { base: main, trackers });

describe("maintainers", () => {
	it("decide who merges a tracker — the owner too", async () => {
		define("access", "web", 'audience = "project-devs"\nsecret = false\nmaintainers = "user:ana"\n');
		const owner = await begin(jack, ["web"]);
		expect(owner.status).toBe(403);
		expect(owner.body.error).toContain("only ana maintains `web`");
		expect((await begin(ana, ["web"])).status).toBe(200);
	});

	it("default to the audience: anyone who may read a tracker may change it", async () => {
		define("access", "web", 'audience = "project-devs"\nsecret = false\n');
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
			'audience = "project-devs"\nsecret = false\nmaintainers = "user:ana"\npaths = [".newgit/trackers/web.toml", ".newgit/trackers/docs.toml"]\n',
		);
		define("admin", "web", 'audience = "project-devs"\nsecret = false\nmaintainers = "project-devs"\n');
		// A stale copy in `access` is not what counts: admin owns the file now.
		define("access", "web", 'audience = "user:jack"\nsecret = false\n');
		define("admin", "docs", 'audience = "project-devs"\nsecret = false\n');

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
});
