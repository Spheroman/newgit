import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, fakeCopy, noEndpoint } from "./fake-repos";

// Fewer, faster round trips for a checkpoint and a merge: several passwords
// in one request, definitions read once until a holder's `main` moves, a
// checkpoint that moves the head with it, and root's fork started when an
// instance is made. New tests live in their own file per AGENTS.md's
// "Working in parallel".

let repos: FakeRepos;
let slug: string;
let jack: string;
let copies: number;
const pending: Promise<unknown>[] = [];

const sha = (c: string) => c.repeat(40);
const access = () => `jack.${slug}.t.access`;

beforeEach(async () => {
	repos = new FakeRepos();
	copies = 0;
	slug = `trips-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect((await call("POST", "/v1/projects", { name: slug, trackers: ["root", "access"] })).status).toBe(201);
	repos.write(access(), "root.toml", 'audience = "project-devs"\ngit = true\n');
});

async function call(method: string, path: string, body?: unknown, prefork = false) {
	const copy = fakeCopy(repos);
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${jack}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{
			repos,
			fetcher: fetch,
			waitUntil: (promise) => void pending.push(promise),
			endpoint: noEndpoint,
			prefork,
			copy: (...args) => {
				copies++;
				return copy(...args);
			},
		},
	);
	const answer = { status: response.status, body: (await response.json()) as any };
	await Promise.all(pending.splice(0));
	return answer;
}

const path = (rest: string) => `/v1/projects/jack/${slug}${rest}`;

describe("several passwords in one request", () => {
	it("answers each repo as it would alone, in order", async () => {
		const asked = await call("POST", path("/credentials"), {
			repos: [`jack.${slug}.t.root`, `jack.${slug}.t.nope`, `jack.${slug}.t.access`],
			scope: "read",
		});
		expect(asked.status).toBe(200);
		const [root, nope, defs] = asked.body.credentials;
		expect(root).toMatchObject({ repo: `jack.${slug}.t.root`, scope: "read" });
		expect(nope).toMatchObject({ repo: `jack.${slug}.t.nope`, status: 404 });
		expect(defs).toMatchObject({ repo: access(), scope: "read" });
		const alone = await call("POST", path("/credentials"), { repo: `jack.${slug}.t.root`, scope: "read" });
		expect(alone.body.password, "the same password it would hand alone").toBe(root.password);
	});

	it("refuses an empty list, or one too long", async () => {
		expect((await call("POST", path("/credentials"), { repos: [] })).status).toBe(400);
		const many = Array.from({ length: 51 }, () => `jack.${slug}.t.root`);
		expect((await call("POST", path("/credentials"), { repos: many })).status).toBe(400);
	});
});

describe("definitions", () => {
	it("are read once while `access`'s `main` stays put, and again the moment it moves", async () => {
		repos.setRef(access(), "refs/heads/main", sha("a"));
		repos.write(access(), "notes.toml", 'audience = "project-devs"\n');
		const ask = () => call("POST", path("/credentials"), { repo: `jack.${slug}.t.notes`, scope: "write" });
		expect((await ask()).status).toBe(200);
		const read = repos.fileReads;
		expect((await ask()).status).toBe(200);
		expect(repos.fileReads, "nothing read again").toBe(read);

		// Tightened on `main`: the next request reads it, and refuses.
		repos.write(access(), "notes.toml", 'audience = "user:ana"\n');
		repos.setRef(access(), "refs/heads/main", sha("b"));
		const refused = await ask();
		expect(refused.status).toBe(403);
		expect(repos.fileReads).toBeGreaterThan(read);
	});

	it("are not kept when a holder has no `main` to know them by", async () => {
		const ask = () => call("POST", path("/credentials"), { repo: `jack.${slug}.t.root`, scope: "read" });
		await ask();
		const read = repos.fileReads;
		await ask();
		expect(repos.fileReads).toBeGreaterThan(read);
	});
});

describe("a checkpoint", () => {
	it("moves the instance's head in the same request", async () => {
		expect((await call("POST", path("/instances"), { name: "one", base_rev: sha("a") })).status).toBe(201);
		const saved = await call("PUT", path("/checkpoints/one/ckpt_001"), { record: { id: "ckpt_001" }, head_rev: sha("c") });
		expect(saved.status).toBe(201);
		expect((await call("GET", path("/instances/one"))).body.headRev).toBe(sha("c"));
	});
});

describe("a new instance", () => {
	it("has its fork of root started when it is made", async () => {
		expect((await call("POST", path("/instances"), { name: "two", base_rev: sha("a") }, true)).status).toBe(201);
		expect(copies).toBe(1);
		expect(repos.repos.has(`jack.${slug}.i.two.root`)).toBe(true);
		const forked = await call("POST", path("/instances/two/trackers/root/fork"), undefined, true);
		expect(forked.status).toBe(200);
		expect(forked.body.repo).toBe(`jack.${slug}.i.two.root`);
		expect(copies, "the first checkpoint finds it made").toBe(1);
	});

	it("has none started where pre-forking is off", async () => {
		expect((await call("POST", path("/instances"), { name: "three", base_rev: sha("a") })).status).toBe(201);
		expect(copies).toBe(0);
	});
});
