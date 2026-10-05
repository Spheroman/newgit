import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos } from "./fake-repos";

// A newgit token's revocation reaches the repo passwords it already minted:
// Artifacts checks those itself, so without this a revoked agent keeps Git
// access until they expire (up to 15 minutes).

let repos: FakeRepos;
let pending: Promise<unknown>[];
let project: string;
let jack: string;

beforeEach(async () => {
	repos = new FakeRepos();
	pending = [];
	project = `acme-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	expect((await call(jack, "POST", "/v1/projects", { name: project })).status).toBe(201);
});

async function call(token: string, method: string, path: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		env,
		{ repos, fetcher: fetch, waitUntil: (promise) => pending.push(promise) },
	);
	return { status: response.status, body: (await response.json()) as any };
}

/** The Artifacts ids of the repo passwords a token was handed. */
async function passwordsFor(token: string, ...repoNames: string[]) {
	const ids: string[] = [];
	for (const repo of repoNames) {
		expect((await call(token, "POST", `/v1/projects/${project}/credentials`, { repo, scope: "read" })).status).toBe(200);
		ids.push(repos.minted.at(-1)!.id);
	}
	return ids;
}

describe("revoking a newgit token", () => {
	it("revokes the repo passwords it minted, and only those", async () => {
		const agent = (await call(jack, "POST", "/v1/tokens", { name: "agent-3" })).body;
		const other = (await call(jack, "POST", "/v1/tokens", { name: "agent-4" })).body;
		const leaked = await passwordsFor(agent.token, `${project}.src`, `${project}.src`);
		const kept = await passwordsFor(other.token, `${project}.src`);

		const revoked = await call(jack, "DELETE", `/v1/tokens/${agent.id}`);
		expect(revoked.body.repo_tokens_revoked).toBe(2);
		for (const id of leaked) expect(repos.revokedTokens.has(id)).toBe(true);
		for (const id of kept) expect(repos.revokedTokens.has(id)).toBe(false);
	});

	it("removing an instance revokes its workspace's repo passwords", async () => {
		const spawned = (await call(jack, "POST", `/v1/projects/${project}/instances`, { name: "attempt-1", base_rev: "a" })).body;
		await Promise.all(pending);
		const ids = await passwordsFor(spawned.token, `${project}.i.attempt-1`);
		expect((await call(jack, "DELETE", `/v1/projects/${project}/instances/attempt-1`)).status).toBe(200);
		for (const id of ids) expect(repos.revokedTokens.has(id)).toBe(true);
	});

	it("renewal does not cut off a push in flight", async () => {
		const spawned = (await call(jack, "POST", `/v1/projects/${project}/instances`, { name: "attempt-1", base_rev: "a" })).body;
		await Promise.all(pending);
		const ids = await passwordsFor(spawned.token, `${project}.src`);
		expect((await call(spawned.token, "POST", "/v1/tokens/renew")).status).toBe(201);
		for (const id of ids) expect(repos.revokedTokens.has(id)).toBe(false);
	});
});
