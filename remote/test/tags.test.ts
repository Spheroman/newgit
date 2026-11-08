import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import type { EndpointKind, EndpointPlan } from "../src/endpoint";
import { type Deps, handle, issue, tagNameProblem } from "../src/index";
import { env } from "cloudflare:workers";
import { call, deliver, deps, jack, main, name, path, plans, push, secretEnv, setAnswers, sha, stub } from "./webhook-fixture";

// Tags: a name for one root commit on root's `main`, frozen with what it pins.
// Made by a push from the project root (`PUT /tags/<name>`) or by a tag made on
// a mirror (the webhook), and served by the Git endpoint as `refs/tags/<name>`.
// New tests live in their own file per AGENTS.md's "Working in parallel".

const tag = (name: string) => path(`/tags/${encodeURIComponent(name)}`);

describe("making a tag", () => {
	it("tags a commit on `main`, and the same tag again is a retry", async () => {
		const made = await call(jack, "PUT", tag("v1.0.0"), { root: main.root });
		expect(made.status).toBe(201);
		expect(made.body).toMatchObject({ name: "v1.0.0", list: { root: main.root }, annotation: null, createdBy: "jack" });
		expect((await call(jack, "PUT", tag("v1.0.0"), { root: main.root })).status).toBe(200);
		expect((await call(jack, "GET", path("/tags"))).body.tags.map((found: { name: string }) => found.name)).toEqual([
			"v1.0.0",
		]);
		expect((await call(jack, "GET", tag("v1.0.0"))).body.list.root).toBe(main.root);
	});

	it("never moves a tag, nor changes what it says", async () => {
		await call(jack, "PUT", tag("v1"), { root: main.root });
		const moved = await call(jack, "PUT", tag("v1"), { root: sha("f") });
		expect(moved.status).toBe(409);
		expect(moved.body.error).toContain("never moves");
		const annotated = await call(jack, "PUT", tag("v1"), {
			root: main.root,
			annotation: { tagger: "Jack <jack@example.com> 1700000000 +0000", message: "one\n" },
		});
		expect(annotated.status).toBe(409);
		expect((await stub().tag("v1"))?.list.root).toBe(main.root);
	});

	it("keeps an annotated tag's tagger and message", async () => {
		const annotation = { tagger: "Jack Wen <jack@example.com> 1700000000 -0800", message: "Release 1.0\n\nNotes.\n" };
		const made = await call(jack, "PUT", tag("release/1.0"), { root: main.root, annotation });
		expect(made.status).toBe(201);
		expect((await stub().tag("release/1.0"))?.annotation).toEqual(annotation);
	});

	it("refuses a commit that is not on `main`, saying to push it there", async () => {
		const offMain: Deps = {
			...deps(),
			ancestry: async (_project, plan) =>
				plan.checks.map((check) => ({ tracker: check.tracker, forward: true, onMain: false })),
		};
		const response = await handle(
			new Request(`https://newgit.dev${tag("v2")}`, {
				method: "PUT",
				headers: { "content-type": "application/json", authorization: `Bearer ${jack}` },
				body: JSON.stringify({ root: sha("f") }),
			}),
			secretEnv,
			offMain,
		);
		expect(response.status).toBe(400);
		expect(((await response.json()) as { error: string }).error).toContain("git push origin main v2");
		expect(await stub().tag("v2")).toBeNull();
	});

	it("refuses what Git would not take", async () => {
		expect((await call(jack, "PUT", tag("v1..2"), { root: main.root })).status).toBe(400);
		expect((await call(jack, "PUT", tag("v1"), { root: "abc" })).status).toBe(400);
		expect((await call(jack, "PUT", tag("v1"), { root: main.root, annotation: { tagger: "nobody", message: "" } })).status).toBe(
			400,
		);
		for (const bad of ["", "-v1", "v1.lock", "a b", "v1/", ".hidden", "x@{y", "a\\b", "a:b"])
			expect(tagNameProblem(bad), bad).not.toBeNull();
		for (const good of ["v0.9.0", "release/1.0", "v1-rc.1"]) expect(tagNameProblem(good), good).toBeNull();
	});

	it("is a person's to make, and someone who may merge into root", async () => {
		const agent = (await issue(env, { kind: "agent", login: "jack", agent: "claude", ttlSeconds: 600 })).token;
		expect((await call(agent, "PUT", tag("v1"), { root: main.root })).status).toBe(403);
		const ana = (await issue(env, { kind: "login", login: "ana", ttlSeconds: LOGIN_TTL_SECONDS })).token;
		expect((await call(ana, "PUT", tag("v1"), { root: main.root })).status).toBe(403);
		expect(await stub().tag("v1")).toBeNull();
	});

	it("is deleted by whoever may make it", async () => {
		await call(jack, "PUT", tag("v1"), { root: main.root });
		expect((await call(jack, "DELETE", tag("v1"))).status).toBe(200);
		expect(await stub().tag("v1")).toBeNull();
		expect((await call(jack, "DELETE", tag("v1"))).status).toBe(404);
	});
});

describe("a tag made on a mirror", () => {
	it("comes in as the root commit the container finds", async () => {
		setAnswers([{ outcome: "tagged", root: main.root, annotation: null }]);
		const answer = await deliver({ payload: push({ ref: "refs/tags/v0.9.0", after: sha("c") }) });
		expect(answer.status).toBe(202);
		expect(plans.at(-1)).toMatchObject({ tag: "v0.9.0", mirror: { commit: sha("c") } });
		expect((await stub().tag("v0.9.0"))?.list.root).toBe(main.root);
		expect((await stub().tag("v0.9.0"))?.createdBy).toBe("jack");
		expect(await stub().pulls()).toEqual([]);
	});

	it("finds our own export's tag already made, and changes nothing", async () => {
		await call(jack, "PUT", tag("v1"), { root: main.root });
		setAnswers([{ outcome: "tagged", root: main.root, annotation: null }]);
		await deliver({ payload: push({ ref: "refs/tags/v1", after: sha("c") }) });
		expect((await stub().tags()).length).toBe(1);
		expect(await stub().pulls()).toEqual([]);
	});

	it("leaves a tag that would move as it is", async () => {
		await call(jack, "PUT", tag("v1"), { root: main.root });
		setAnswers([{ outcome: "tagged", root: sha("f"), annotation: null }]);
		await deliver({ payload: push({ ref: "refs/tags/v1", after: sha("c") }) });
		expect((await stub().tag("v1"))?.list.root).toBe(main.root);
		expect(await stub().pulls()).toEqual([]);
	});

	it("is refused when the container cannot find its commit on `main`", async () => {
		setAnswers([{ outcome: "refused", why: "not on `main`" }]);
		await deliver({ payload: push({ ref: "refs/tags/v1", after: sha("c") }) });
		expect(await stub().tag("v1")).toBeNull();
		expect(await stub().pulls()).toEqual([]);
	});

	it("ignores a tag deleted there: tags never move", async () => {
		const answer = await deliver({ payload: push({ ref: "refs/tags/v1", deleted: true, after: "0".repeat(40) }) });
		expect(answer.status).toBe(200);
		expect(await stub().pulls()).toEqual([]);
	});

	it("needs a pusher who may merge into root", async () => {
		const answer = await deliver({ payload: push({ ref: "refs/tags/v1", after: sha("c"), pusher: { name: "mallory" } }) });
		expect(answer.status).toBe(403);
		expect(await stub().pulls()).toEqual([]);
	});
});

describe("the Git endpoint", () => {
	it("serves each tag, cut to what its reader may read", async () => {
		await stub().createTag("v1", { ...main, secret: sha("9") }, null, "jack");
		const served: EndpointPlan[] = [];
		const capture: Deps = {
			...deps(),
			endpoint: async (_project: string, _kind: EndpointKind, plan: EndpointPlan) => {
				served.push(plan);
				return new Response("served");
			},
		};
		const response = await handle(
			new Request(`https://newgit.dev/jack/${name}.git/info/refs?service=git-upload-pack`, {
				headers: { authorization: `Basic ${btoa(`x:${jack}`)}` },
			}),
			secretEnv,
			capture,
		);
		expect(response.status).toBe(200);
		expect(served[0].tags).toEqual({ v1: { list: main, annotation: null } });
	});
});
