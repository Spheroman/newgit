import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { handle } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

const get = (path: string, method = "GET") =>
	handle(new Request(`https://newgit.dev${path}`, { method }), env, {
		repos: new FakeRepos(),
		fetcher: fetch,
		waitUntil: () => {},
		endpoint: noEndpoint,
	});

describe("the installer", () => {
	it("is served without a token", async () => {
		for (const path of ["/install.sh", "/install"]) {
			const response = await get(path);
			expect(response.status).toBe(200);
			expect(response.headers.get("content-type")).toMatch(/^text\/x-shellscript/);
			const body = await response.text();
			expect(body.startsWith("#!/bin/sh\n")).toBe(true);
			// A truncated download must run nothing: main is called last.
			expect(body.trimEnd().endsWith('main "$@"')).toBe(true);
		}
	});

	it("answers HEAD with no body", async () => {
		const response = await get("/install.sh", "HEAD");
		expect(response.status).toBe(200);
		expect(await response.text()).toBe("");
	});
});
