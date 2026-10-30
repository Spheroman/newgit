import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { handle } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

const PAGE = { project: "spheroman/newgit", tracker: "site", path: "remote/site/index.html" };
const REPO = "spheroman.newgit.t.site";

describe("the landing page's ref", () => {
	it("is the tracker repo's branch `main`, which `tracker merge` moves", async () => {
		const repos = new FakeRepos();
		await repos.create(REPO);
		repos.write(REPO, PAGE.path, "<!doctype html>");
		const response = await handle(new Request("https://newgit.dev/"), { ...env, LANDING_PAGE: PAGE } as typeof env, {
			repos,
			fetcher: fetch,
			waitUntil: () => {},
			endpoint: noEndpoint,
		});
		expect(response.status).toBe(200);
		// Not `HEAD`: Artifacts resolves only branches, tags, and commit ids,
		// and a repo whose `main` was never merged has no page to serve.
		expect(repos.reads).toEqual([{ name: REPO, ref: "main", path: PAGE.path }]);
	});
});
