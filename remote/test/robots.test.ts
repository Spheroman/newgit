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

describe("robots.txt", () => {
	it("lets every crawler read everything, without a token", async () => {
		const response = await get("/robots.txt");
		expect(response.status).toBe(200);
		expect(response.headers.get("content-type")).toMatch(/^text\/plain/);
		const lines = (await response.text()).split("\n").map((line) => line.trim());
		expect(lines).toContain("User-agent: *");
		expect(lines).toContain("Allow: /");
		expect(lines.filter((line) => /^Disallow:\s*\S/.test(line))).toEqual([]);
	});

	it("answers HEAD with no body", async () => {
		const response = await get("/robots.txt", "HEAD");
		expect(response.status).toBe(200);
		expect(await response.text()).toBe("");
	});
});
