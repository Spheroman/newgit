// The site: the landing page at `/` and the wiki under `/docs`, both read
// from trackers at their `main` on every edge-cache miss, so publishing is a
// merge and never a deploy (docs/design/remote.md, "The site").
//
// `SITE` in wrangler.jsonc names exactly what is published. The look (the
// landing page and the wiki's page template) is files in one tracker; the
// wiki's words are Markdown in another, a directory of it. Nothing else in
// either tracker is served: a name that is not a plain page name, `_sidebar`
// included, is a 404 without a read.

import { Marked, type Tokens } from "marked";

import type { Repos } from "./artifacts";
import { parseProjectKey, repoName } from "./names";

/** One file of one tracker of the site's project. */
interface TrackerFile {
	tracker: string;
	path: string;
}

/** `SITE` in wrangler.jsonc. */
export interface SiteConfig {
	project: string;
	/** Served at `/`. */
	landing?: TrackerFile;
	/** The wiki: `dir/<page>.md` at `/docs/<page>`, `dir/README.md` at `/docs`. */
	docs?: {
		tracker: string;
		dir: string;
		/** The page every wiki page is poured into: `{{title}}`, `{{nav}}`, `{{content}}`, `{{source}}`. */
		template: TrackerFile;
		/** Where the wiki's files are browsable (a GitHub `blob/main`): edit links, and links out of the wiki. */
		source: string;
	};
}

/** Where `/` sends a visitor when this remote publishes no landing page. */
export const REPOSITORY = "https://github.com/Spheroman/newgit";

/** Where the wiki lives on the site. */
const DOCS = "/docs";

/** A page's name: what `/docs/<name>` may say, and so which files are pages. */
const PAGE = /^[a-z0-9][a-z0-9-]*$/;

/** Paths the edge keeps (`fetch` in src/index.ts), for as long as the response says. */
export function isSitePath(path: string): boolean {
	return path === "/" || path === DOCS || path.startsWith(`${DOCS}/`);
}

/**
 * The landing page: one file of one tracker, at that tracker's `main`. newgit.dev's
 * is in `site`, a tracker outside Git, so this route is the one place its
 * content is published, and only the file the config names. Only a merge
 * creates `main`, so a tracker never merged serves nothing. Where the site
 * has a wiki, the landing page carries its sidebar: `{{nav}}`, and no other
 * slot, is filled as on every wiki page.
 */
export async function landingPage(method: string, site: SiteConfig | undefined, repos: Repos): Promise<Response> {
	if (!site?.landing) return Response.redirect(REPOSITORY, 302);
	const [html, nav] = await Promise.all([
		read(repos, site.project, site.landing.tracker, site.landing.path),
		sidebar(site, repos, "/"),
	]);
	if (html === null) return Response.redirect(REPOSITORY, 302);
	return page(method, html.replace(/\{\{nav\}\}/g, () => nav), 200);
}

/** The wiki's sidebar as HTML, with the link to `here` marked current; empty without one. */
async function sidebar(site: SiteConfig, repos: Repos, here: string): Promise<string> {
	const docs = site.docs;
	if (!docs) return "";
	const markdown = await read(repos, site.project, docs.tracker, `${docs.dir}/_sidebar.md`);
	return markdown === null ? "" : render(markdown, { dir: docs.dir, source: sourceOf(docs), here }).html;
}

function sourceOf(docs: NonNullable<SiteConfig["docs"]>): string {
	return docs.source.replace(/\/+$/, "");
}

/** A wiki page, `slug` being what follows `/docs/` (empty for the wiki's home). */
export async function docsPage(
	method: string,
	site: SiteConfig | undefined,
	repos: Repos,
	slug: string,
): Promise<Response | null> {
	const docs = site?.docs;
	if (!site || !docs) return null;
	if (slug !== "" && !PAGE.test(slug)) return notFound(method);
	const file = slug === "" ? "README" : slug;
	const here = slug === "" ? DOCS : `${DOCS}/${slug}`;
	const [template, markdown, nav] = await Promise.all([
		read(repos, site.project, docs.template.tracker, docs.template.path),
		read(repos, site.project, docs.tracker, `${docs.dir}/${file}.md`),
		sidebar(site, repos, here),
	]);
	if (template === null) return notFound(method);
	const links = { dir: docs.dir, source: sourceOf(docs), here };
	const body = markdown === null ? null : render(markdown, links);
	const html = fill(template, {
		title: escapeHtml(body?.title ?? "Not found"),
		nav,
		content: body?.html ?? `<h1>Not found</h1><p>No page is called <code>${escapeHtml(slug)}</code>. <a href="${DOCS}">The wiki's home</a> lists every page.</p>`,
		source: escapeHtml(`${links.source}/${docs.dir}/${file}.md`),
	});
	return page(method, html, body === null ? 404 : 200);
}

async function read(repos: Repos, project: string, tracker: string, path: string): Promise<string | null> {
	const id = parseProjectKey(project);
	return id ? repos.readFile(repoName({ kind: "tracker", project: id, tracker }), "HEAD", path) : null;
}

function page(method: string, html: string, status: number): Response {
	return new Response(method === "HEAD" ? null : html, {
		status,
		headers: { "content-type": "text/html; charset=utf-8", "cache-control": "public, max-age=60" },
	});
}

function notFound(method: string): Response {
	return new Response(method === "HEAD" ? null : "not found\n", {
		status: 404,
		headers: { "content-type": "text/plain; charset=utf-8" },
	});
}

/** Every slot in one pass, so a page that says `{{nav}}` says it literally. */
function fill(template: string, slots: Record<"title" | "nav" | "content" | "source", string>): string {
	return template.replace(/\{\{(title|nav|content|source)\}\}/g, (_, name: keyof typeof slots) => slots[name]);
}

interface Links {
	/** The wiki's directory in its tracker, which relative links resolve against. */
	dir: string;
	source: string;
	/** The page being rendered, whose link in the sidebar is marked current. */
	here: string;
}

/**
 * Markdown to HTML, with its first `#` heading as the title. Links are
 * written for GitHub, file to file, so the wiki reads the same in either
 * place: one to another page becomes that page's address here, one to
 * anything else in the repository goes to `source`. Headings get GitHub's
 * ids, so `commands.md#remote` lands on the same heading in both.
 *
 * Raw HTML in the Markdown passes through: the words are as trusted as the
 * template, merged by the same maintainers.
 */
function render(markdown: string, links: Links): { title: string | null; html: string } {
	let title: string | null = null;
	const ids = new Map<string, number>();
	const marked = new Marked({
		gfm: true,
		renderer: {
			heading({ tokens, depth, text }) {
				const plain = tokens.map((token) => ("text" in token ? token.text : token.raw)).join("") || text;
				if (depth === 1 && title === null) title = plain;
				const id = uniqueId(githubSlug(plain), ids);
				return `<h${depth} id="${escapeHtml(id)}">${this.parser.parseInline(tokens)}</h${depth}>\n`;
			},
			link({ href, title: hover, tokens }: Tokens.Link) {
				const target = resolve(href, links);
				const current = target === links.here ? ' aria-current="page"' : "";
				const titled = hover ? ` title="${escapeHtml(hover)}"` : "";
				return `<a href="${escapeHtml(target)}"${titled}${current}>${this.parser.parseInline(tokens)}</a>`;
			},
			image({ href, title: hover, text }: Tokens.Image) {
				const target = resolve(href, links);
				// A file in the repository, as GitHub serves its bytes.
				const src = target.startsWith(`${links.source}/`) ? `${target}?raw=true` : target;
				const titled = hover ? ` title="${escapeHtml(hover)}"` : "";
				return `<img src="${escapeHtml(src)}" alt="${escapeHtml(text)}"${titled}>`;
			},
		},
	});
	const html = marked.parse(markdown, { async: false });
	return { title, html };
}

/** Where a link written in a wiki file goes from the site. */
export function resolve(href: string, links: Pick<Links, "dir" | "source">): string {
	// A URL, a site path, or a heading on this page: as written.
	if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("/") || href.startsWith("#") || href === "") return href;
	const hash = href.indexOf("#");
	const [path, fragment] = hash === -1 ? [href, ""] : [href.slice(0, hash), href.slice(hash)];
	const parts: string[] = [];
	for (const part of `${links.dir}/${path.split("?")[0]}`.split("/")) {
		if (part === "" || part === ".") continue;
		if (part === "..") {
			if (parts.pop() === undefined) return href;
		} else parts.push(part);
	}
	const full = parts.join("/");
	if (full === links.dir) return `${DOCS}${fragment}`;
	const inDir = full.startsWith(`${links.dir}/`) ? full.slice(links.dir.length + 1) : null;
	if (inDir === "README.md") return `${DOCS}${fragment}`;
	const name = inDir?.endsWith(".md") ? inDir.slice(0, -3) : null;
	if (name !== null && PAGE.test(name)) return `${DOCS}/${name}${fragment}`;
	return `${links.source}/${full}${fragment}`;
}

/** GitHub's heading id: lowercased, punctuation dropped, spaces to hyphens. */
export function githubSlug(text: string): string {
	return text
		.toLowerCase()
		.replace(/[^\p{L}\p{N}\p{M} _-]/gu, "")
		.replace(/ /g, "-");
}

function uniqueId(slug: string, seen: Map<string, number>): string {
	const count = seen.get(slug) ?? 0;
	seen.set(slug, count + 1);
	return count === 0 ? slug : `${slug}-${count}`;
}

function escapeHtml(text: string): string {
	return text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}
