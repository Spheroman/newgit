import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { handle } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

const PAGE = { project: "spheroman/newgit", tracker: "site", path: "remote/site/index.html" };
const REPO = "spheroman.newgit.t.site";

const get = (repos: FakeRepos, page: typeof PAGE | undefined, path = "/", method = "GET") =>
	handle(new Request(`https://newgit.dev${path}`, { method }), { ...env, LANDING_PAGE: page } as typeof env, {
		repos,
		fetcher: fetch,
		waitUntil: () => {},
		endpoint: noEndpoint,
	});

describe("the landing page", () => {
	it("is the named file of the named tracker, without a token", async () => {
		const repos = new FakeRepos();
		await repos.create(REPO);
		repos.write(REPO, PAGE.path, "<!doctype html><title>newgit</title>");
		const response = await get(repos, PAGE);
		expect(response.status).toBe(200);
		expect(response.headers.get("content-type")).toBe("text/html; charset=utf-8");
		expect(await response.text()).toBe("<!doctype html><title>newgit</title>");
	});

	it("answers HEAD with no body", async () => {
		const repos = new FakeRepos();
		await repos.create(REPO);
		repos.write(REPO, PAGE.path, "<!doctype html>");
		const response = await get(repos, PAGE, "/", "HEAD");
		expect(response.status).toBe(200);
		expect(await response.text()).toBe("");
	});

	it("publishes nothing else in the tracker", async () => {
		const repos = new FakeRepos();
		await repos.create(REPO);
		repos.write(REPO, PAGE.path, "<!doctype html>");
		repos.write(REPO, "remote/site/draft.html", "not yet");
		const response = await get(repos, PAGE, "/remote/site/draft.html");
		expect(response.status).toBe(401);
	});

	it("sends a visitor to the repository when there is no page to serve", async () => {
		for (const page of [undefined, PAGE]) {
			const response = await get(new FakeRepos(), page);
			expect(response.status).toBe(302);
			expect(response.headers.get("location")).toBe("https://github.com/Spheroman/newgit");
		}
	});
});
