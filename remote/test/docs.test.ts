import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { handle } from "../src/index";
import { type SiteConfig, githubSlug, resolve } from "../src/site";
import { FakeRepos, noEndpoint } from "./fake-repos";

const SOURCE = "https://github.com/Spheroman/newgit/blob/main";
const SITE: SiteConfig = {
	project: "spheroman/newgit",
	docs: {
		tracker: "root",
		dir: "docs/wiki",
		template: { tracker: "site", path: "remote/site/page.html" },
		source: SOURCE,
	},
};
const ROOT = "spheroman.newgit.t.root";
const LOOK = "spheroman.newgit.t.site";
const TEMPLATE = "<title>{{title}}</title><nav>{{nav}}</nav><main>{{content}}</main><a href=\"{{source}}\">edit</a>";

async function wiki(pages: Record<string, string>, template: string | null = TEMPLATE) {
	const repos = new FakeRepos();
	await repos.create(ROOT);
	await repos.create(LOOK);
	if (template !== null) repos.write(LOOK, "remote/site/page.html", template);
	for (const [name, text] of Object.entries(pages)) repos.write(ROOT, `docs/wiki/${name}`, text);
	return repos;
}

const get = (repos: FakeRepos, path: string, site: SiteConfig | undefined = SITE, method = "GET") =>
	handle(new Request(`https://newgit.dev${path}`, { method }), { ...env, SITE: site } as typeof env, {
		repos,
		fetcher: fetch,
		waitUntil: () => {},
		endpoint: noEndpoint,
	});

describe("the wiki", () => {
	it("serves README.md at /docs and a page at /docs/<name>, in the template", async () => {
		const repos = await wiki({ "README.md": "# Wiki\n\nStart here.", "commands.md": "# Commands\n\n- `newgit init`" });
		const home = await get(repos, "/docs");
		expect(home.status).toBe(200);
		expect(home.headers.get("content-type")).toBe("text/html; charset=utf-8");
		const html = await home.text();
		expect(html).toContain("<title>Wiki</title>");
		expect(html).toContain("<p>Start here.</p>");
		expect(html).toContain(`href="${SOURCE}/docs/wiki/README.md"`);

		const page = await (await get(repos, "/docs/commands/")).text();
		expect(page).toContain("<title>Commands</title>");
		expect(page).toContain("<code>newgit init</code>");
		expect(page).toContain(`href="${SOURCE}/docs/wiki/commands.md"`);
	});

	it("puts the sidebar in the nav, marking the page being read", async () => {
		const repos = await wiki({
			"README.md": "# Wiki",
			"commands.md": "# Commands",
			"_sidebar.md": "- [Home](README.md)\n- [Commands](commands.md)",
		});
		const html = await (await get(repos, "/docs/commands")).text();
		expect(html).toContain('<a href="/docs">Home</a>');
		expect(html).toContain('<a href="/docs/commands" aria-current="page">Commands</a>');
	});

	it("turns links written for GitHub into the site's, and the rest into the repository's", async () => {
		const repos = await wiki({
			"README.md": [
				"# Wiki",
				"[a](commands.md#remote) [b](../design/spec.md#render) [c](https://example.com) [d](#here) [e](./)",
			].join("\n\n"),
		});
		const html = await (await get(repos, "/docs")).text();
		expect(html).toContain('<a href="/docs/commands#remote">a</a>');
		expect(html).toContain(`<a href="${SOURCE}/docs/design/spec.md#render">b</a>`);
		expect(html).toContain('<a href="https://example.com">c</a>');
		expect(html).toContain('<a href="#here">d</a>');
		expect(html).toContain('<a href="/docs" aria-current="page">e</a>');
	});

	it("gives headings GitHub's ids", async () => {
		const repos = await wiki({ "README.md": "# Wiki\n\n## Using it with T3 Code\n\n## `export` fails closed\n\n## Using it with T3 Code" });
		const html = await (await get(repos, "/docs")).text();
		expect(html).toContain('<h2 id="using-it-with-t3-code">');
		expect(html).toContain('<h2 id="export-fails-closed">');
		expect(html).toContain('<h2 id="using-it-with-t3-code-1">');
	});

	it("answers a missing page with 404 in the template", async () => {
		const repos = await wiki({ "README.md": "# Wiki" });
		const response = await get(repos, "/docs/nope");
		expect(response.status).toBe(404);
		expect(await response.text()).toContain("<title>Not found</title>");
	});

	it("publishes nothing that is not a page: no sidebar, no nesting, no other file", async () => {
		const repos = await wiki({ "README.md": "# Wiki", "_sidebar.md": "- secret" });
		repos.write(ROOT, "docs/wiki/notes/draft.md", "# Draft");
		repos.write(ROOT, ".env", "KEY=1");
		const before = repos.fileReads;
		for (const path of ["/docs/_sidebar", "/docs/notes/draft", "/docs/..%2F..%2F.env", "/docs/README.md"]) {
			const response = await get(repos, path);
			expect(response.status, path).toBe(404);
			expect(await response.text(), path).not.toContain("secret");
		}
		expect(repos.fileReads).toBe(before);
	});

	it("answers HEAD with no body", async () => {
		const repos = await wiki({ "README.md": "# Wiki" });
		const response = await get(repos, "/docs", SITE, "HEAD");
		expect(response.status).toBe(200);
		expect(await response.text()).toBe("");
	});

	it("is not served when the remote names no wiki, or its template was never merged", async () => {
		const repos = await wiki({ "README.md": "# Wiki" });
		// Not a page at all, then: the API's answer to a path it does not know.
		expect((await get(repos, "/docs", { project: SITE.project })).status).toBe(401);
		expect((await get(await wiki({ "README.md": "# Wiki" }, null), "/docs")).status).toBe(404);
	});

	it("leaves a page's text alone where it looks like a slot", async () => {
		const repos = await wiki({ "README.md": "# Wiki\n\nWrite `{{nav}}` in the template." });
		const html = await (await get(repos, "/docs")).text();
		expect(html).toContain("<code>{{nav}}</code>");
	});
});

describe("the landing page, beside a wiki", () => {
	it("carries the wiki's sidebar, with Home marked current, and fills no other slot", async () => {
		const repos = await wiki({ "_sidebar.md": "- [Home](/)\n- [Overview](README.md)" });
		repos.write(LOOK, "remote/site/index.html", "<nav>{{nav}}</nav><p>{{title}}</p>");
		const site = { ...SITE, landing: { tracker: "site", path: "remote/site/index.html" } };
		const html = await (await get(repos, "/", site)).text();
		expect(html).toContain('<a href="/" aria-current="page">Home</a>');
		expect(html).toContain('<a href="/docs">Overview</a>');
		expect(html).toContain("<p>{{title}}</p>");
	});
});

describe("resolve", () => {
	const links = { dir: "docs/wiki", source: SOURCE };
	it("leaves a link above the repository as written", () => {
		expect(resolve("../../../x.md", links)).toBe("../../../x.md");
	});
	it("sends README.md to the wiki's home", () => {
		expect(resolve("README.md#top", links)).toBe("/docs#top");
		expect(resolve("../../README.md", links)).toBe(`${SOURCE}/README.md`);
	});
});

describe("githubSlug", () => {
	it("matches GitHub's", () => {
		expect(githubSlug("What installs cost")).toBe("what-installs-cost");
		expect(githubSlug("`[[render]]` puts ports")).toBe("render-puts-ports");
		expect(githubSlug("Trackers outside Git")).toBe("trackers-outside-git");
	});
});
