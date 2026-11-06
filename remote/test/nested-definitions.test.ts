import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue, loadDefinitions } from "../src/index";
import { repoName } from "../src/names";
import { FakeRepos, noEndpoint } from "./fake-repos";

// A tracker carved inside a mounted tracker in Git is defined in that
// tracker's own `.newgit/trackers/` (#201), not in `access`.

const project = { owner: "jack", name: "web" };
const repo = (tracker: string) => repoName({ kind: "tracker", project, tracker });
const definition = (audience: string, mount: string, extra = "") =>
	`audience = "${audience}"\nstorage = "local"\ngit = true\nmount = "${mount}"\npaths = ["."]\n${extra}`;

async function repos(trackers: string[] = []) {
	const fake = new FakeRepos();
	for (const tracker of ["root", "access", "t3code", ...trackers]) await fake.create(repo(tracker));
	fake.write(repo("access"), "t3code.toml", definition("project-devs", "apps/t3code"));
	return fake;
}

describe("a tracker defined inside another", () => {
	it("is found in its parent, mounted relative to it, and held by it", async () => {
		const fake = await repos();
		fake.write(repo("t3code"), ".newgit/trackers/vendored.toml", definition("user:ana", ".repos"));
		const facts = await loadDefinitions(fake, project, ["t3code", "vendored"]);
		const vendored = facts.get("vendored");
		expect(vendored?.mount).toBe("apps/t3code/.repos");
		expect(vendored?.source).toBe("t3code");
		expect(vendored?.git).toBe(true);
		expect(vendored?.audience).not.toEqual(facts.get("t3code")?.audience);
	});

	it("is found through a parent that is itself nested", async () => {
		const fake = await repos(["vendored"]);
		fake.write(repo("t3code"), ".newgit/trackers/vendored.toml", definition("project-devs", ".repos"));
		fake.write(repo("vendored"), ".newgit/trackers/effect.toml", definition("project-devs", "effect"));
		const facts = await loadDefinitions(fake, project, ["t3code", "vendored", "effect"]);
		expect(facts.get("effect")?.mount).toBe("apps/t3code/.repos/effect");
		expect(facts.get("effect")?.source).toBe("vendored");
	});

	it("cannot redefine a tracker `access` defines", async () => {
		const fake = await repos();
		fake.write(repo("access"), "billing.toml", definition("project-devs", "billing"));
		fake.write(repo("t3code"), ".newgit/trackers/billing.toml", definition("public", ".billing"));
		const facts = await loadDefinitions(fake, project, ["t3code", "billing"]);
		expect(facts.get("billing")?.mount).toBe("billing");
		expect(facts.get("billing")?.source).toBe("access");
	});

	it("cannot take a name whose definition in `access` does not parse", async () => {
		const fake = await repos();
		fake.write(repo("access"), "billing.toml", "audience = \"nobody real\"\n");
		fake.write(repo("t3code"), ".newgit/trackers/billing.toml", definition("public", ".billing"));
		const facts = await loadDefinitions(fake, project, ["t3code", "billing"]);
		expect(facts.has("billing")).toBe(false);
	});

	// Otherwise whoever maintains a tracker that sorts first decides the
	// audience of a child that is not theirs.
	it("leaves a name two trackers define undefined", async () => {
		const fake = await repos(["plugins"]);
		fake.write(repo("access"), "plugins.toml", definition("project-devs", "apps/plugins"));
		fake.write(repo("t3code"), ".newgit/trackers/vendored.toml", definition("user:ana", ".repos"));
		fake.write(repo("plugins"), ".newgit/trackers/vendored.toml", definition("public", "vendored"));
		const facts = await loadDefinitions(fake, project, ["plugins", "t3code", "vendored"]);
		expect(facts.has("vendored")).toBe(false);
	});

	it("leaves a tracker nobody defines undefined", async () => {
		const facts = await loadDefinitions(await repos(), project, ["t3code", "nowhere"]);
		expect(facts.has("nowhere")).toBe(false);
	});
});

describe("merging a tracker defined inside another", () => {
	const sha = (c: string) => c.repeat(40);

	it("is not refused as undefined, and asks its own definition who may", async () => {
		const name = `web-${crypto.randomUUID().slice(0, 8)}`;
		const fake = new FakeRepos();
		const jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		const ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		const repoOf = (tracker: string) => `jack.${name}.t.${tracker}`;
		const call = async (token: string, method: string, route: string, body?: unknown) => {
			const response = await handle(
				new Request(`https://newgit.dev${route}`, {
					method,
					headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
					body: body === undefined ? undefined : JSON.stringify(body),
				}),
				env,
				{
					repos: fake,
					fetcher: fetch,
					waitUntil: () => {},
					endpoint: noEndpoint,
					ancestry: async (_project: string, plan: { checks: { tracker: string }[] }) =>
						plan.checks.map((check) => ({ tracker: check.tracker, forward: false, onMain: false })),
				},
			);
			return { status: response.status, body: (await response.json()) as any };
		};
		const main = { root: sha("a"), access: sha("b"), t3code: sha("c"), vendored: sha("d") };
		const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;
		await call(jack, "POST", "/v1/projects", { name, trackers: Object.keys(main) });
		await call(jack, "PUT", path("/branches/main"), { list: main });
		await call(jack, "POST", path("/members"), { login: "ana" });
		fake.write(repoOf("access"), "t3code.toml", definition("project-devs", "apps/t3code"));
		fake.write(repoOf("t3code"), ".newgit/trackers/vendored.toml", definition("project-devs", ".repos", 'maintainers = "user:ana"\n'));
		const merge = (token: string) =>
			call(token, "POST", path("/branches/main/merge"), {
				base: main,
				list: { ...main, root: sha("1"), vendored: sha("2") },
			});
		const refused = await merge(jack);
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("only ana maintains `vendored`");
		expect((await merge(ana)).status).toBe(200);
	});
});
