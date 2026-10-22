import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS, invited } from "../src/auth";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

/** GitHub's device flow, answering as `login`. */
const github = (login: string): typeof fetch =>
	(async (input: RequestInfo | URL) => {
		const url = String(input);
		if (url.endsWith("/login/oauth/access_token")) return Response.json({ access_token: "gh-test" });
		if (url.endsWith("/user")) return Response.json({ login });
		return new Response("unexpected", { status: 500 });
	}) as typeof fetch;

async function call(allowed: string | undefined, path: string, init: { token?: string; body?: unknown; as?: string }) {
	const response = await handle(
		new Request(`https://newgit.dev${path}`, {
			method: init.body === undefined ? "GET" : "POST",
			headers: {
				"content-type": "application/json",
				...(init.token ? { authorization: `Bearer ${init.token}` } : {}),
			},
			body: init.body === undefined ? undefined : JSON.stringify(init.body),
		}),
		{ ...env, ALLOWED_LOGINS: allowed },
		{ repos: new FakeRepos(), fetcher: github(init.as ?? "jack"), waitUntil: () => {}, endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as { error?: string; login?: string } };
}

const tokenFor = async (login: string) =>
	(await issue(env, { kind: "login", login, ttlSeconds: LOGIN_TTL_SECONDS })).token;

describe("the allowlist", () => {
	it("reads logins separated by commas or whitespace, ignoring case", () => {
		expect(invited("jack, ana\nbo", "Ana")).toBe(true);
		expect(invited("jack ana", "bo")).toBe(false);
		expect(invited("*", "anyone")).toBe(true);
		expect(invited(undefined, "jack")).toBe(false);
		expect(invited("", "jack")).toBe(false);
	});

	it("refuses to log in someone not invited", async () => {
		const refused = await call("jack", "/v1/login/poll", { body: { device_code: "d" }, as: "eve" });
		expect(refused.status).toBe(403);
		expect(refused.body.error).toContain("`eve` is not invited");
		const admitted = await call("jack", "/v1/login/poll", { body: { device_code: "d" }, as: "Jack" });
		expect(admitted).toMatchObject({ status: 200, body: { login: "jack" } });
	});

	it("shuts out tokens issued before their login was taken off the list", async () => {
		const ana = await tokenFor("ana");
		expect((await call("jack, ana", "/v1/me", { token: ana })).status).toBe(200);
		expect((await call("jack", "/v1/me", { token: ana })).status).toBe(403);
	});

	it("shuts out agents acting for someone not invited", async () => {
		const agent = (await issue(env, { kind: "agent", login: "ana", agent: "a1", ttlSeconds: 3600 })).token;
		expect((await call("jack", "/v1/me", { token: agent })).status).toBe(403);
	});

	it("admits no one when it is not set", async () => {
		const refused = await call(undefined, "/v1/me", { token: await tokenFor("jack") });
		expect(refused.status).toBe(503);
		expect(refused.body.error).toContain("ALLOWED_LOGINS");
	});
});
