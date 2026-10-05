import { describe, expect, it } from "vitest";

import {
	type Audience,
	type Facts,
	type RepoRef,
	decide,
	parseAudience,
	parseProjectKey,
	parseRepoName,
	partProblem,
	projectKey,
	repoName,
} from "../src/names";

const acme = { owner: "jack", name: "acme" };
const member = { login: "jack", member: true };
const teammate = { login: "ana", member: true };
const outsider = { login: "eve", member: false };

describe("project keys", () => {
	it("round-trip, with the owner lowercased", () => {
		expect(projectKey(acme)).toBe("jack/acme");
		expect(parseProjectKey("Jack/acme")).toEqual(acme);
	});

	it("refuse anything that is not <owner>/<name>", () => {
		for (const key of ["acme", "jack/", "/acme", "jack/acme/extra", "jack/a.b", "ja ck/acme", ""])
			expect(parseProjectKey(key), key).toBeNull();
	});
});

describe("repo names", () => {
	const refs: RepoRef[] = [
		{ kind: "tracker", project: acme, tracker: "root" },
		{ kind: "tracker", project: acme, tracker: "runtime-env", login: "ana" },
		{ kind: "fork", project: acme, instance: "fix-login", tracker: "root" },
	];

	it("spell out owner, project, role, and the rest", () => {
		expect(refs.map(repoName)).toEqual([
			"jack.acme.t.root",
			"jack.acme.t.runtime-env.ana",
			"jack.acme.i.fix-login.root",
		]);
	});

	it("round-trip every kind", () => {
		for (const ref of refs) expect(parseRepoName(repoName(ref))).toEqual(ref);
	});

	it("lowercase the owner and a per-user login", () => {
		expect(parseRepoName("Jack.acme.t.env.Ana")).toEqual({
			kind: "tracker",
			project: acme,
			tracker: "env",
			login: "ana",
		});
	});

	it("refuse names that are not one of a project's repos", () => {
		for (const name of [
			"acme",
			"jack.acme",
			"jack.acme.t",
			"jack.acme.t.a.b.c",
			"jack.acme.i.x",
			"jack.acme.i.x.y.z",
			"jack.acme.x.root",
			"jack..t.root",
			".acme.t.root",
			"jack.acme.src",
			"jack.acme.t.root.",
			"jack.acme.t.-root",
			"acme.src",
		])
			expect(parseRepoName(name), name).toBeNull();
	});
});

describe("name parts", () => {
	it("accept letters, digits, _ and -, and say why when not", () => {
		expect(partProblem("tracker name", "runtime_env-2")).toBeNull();
		for (const bad of ["", "a.b", "-a", "a b", "a/b", "x".repeat(64)])
			expect(partProblem("tracker name", bad), bad).toContain("tracker name");
	});
});

describe("audiences", () => {
	it("parses every committed form", () => {
		expect(parseAudience("public")).toEqual({ kind: "public" });
		expect(parseAudience("project-devs")).toEqual({ kind: "project-devs" });
		expect(parseAudience("user")).toEqual({ kind: "user" });
		expect(parseAudience("user:Jack")).toEqual({ kind: "named", login: "jack" });
		expect(parseAudience("user:")).toBeNull();
		expect(parseAudience("everyone")).toBeNull();
	});
});

describe("who may hold which token", () => {
	const tracker = (audience: Audience, code: boolean, login?: string) => ({
		ref: { kind: "tracker", project: acme, tracker: "env", login } as RepoRef,
		facts: { tracker: { audience, code } } as Facts,
	});
	const fork = (audience: Audience, code: boolean, instanceOwner: string | undefined) => ({
		ref: { kind: "fork", project: acme, instance: "fix-login", tracker: "root" } as RepoRef,
		facts: { instanceOwner, tracker: { audience, code } } as Facts,
	});

	it("a code tracker: its audience reads, nobody writes", () => {
		const devs = tracker({ kind: "project-devs" }, true);
		expect(decide(devs.ref, "read", teammate, devs.facts).ok).toBe(true);
		expect(decide(devs.ref, "read", outsider, devs.facts).ok).toBe(false);
		for (const who of [member, teammate])
			expect(decide(devs.ref, "write", who, devs.facts)).toMatchObject({
				ok: false,
				reason: expect.stringContaining("newgit merge"),
			});
		expect(decide(devs.ref, "write", outsider, devs.facts).ok).toBe(false);
		const open = tracker({ kind: "public" }, true);
		expect(decide(open.ref, "read", outsider, open.facts).ok).toBe(true);
		expect(decide(open.ref, "write", member, open.facts).ok).toBe(false);
	});

	it("a shared secret tracker: its audience reads and writes", () => {
		const devs = tracker({ kind: "project-devs" }, false);
		expect(decide(devs.ref, "read", teammate, devs.facts).ok).toBe(true);
		expect(decide(devs.ref, "write", teammate, devs.facts).ok).toBe(true);
		expect(decide(devs.ref, "read", outsider, devs.facts).ok).toBe(false);
		expect(decide(devs.ref, "write", outsider, devs.facts).ok).toBe(false);
	});

	it("a public secret tracker: anyone reads, members write", () => {
		const open = tracker({ kind: "public" }, false);
		expect(decide(open.ref, "read", outsider, open.facts).ok).toBe(true);
		expect(decide(open.ref, "write", outsider, open.facts)).toMatchObject({ ok: false });
		expect(decide(open.ref, "write", teammate, open.facts).ok).toBe(true);
	});

	it("a named secret tracker: only that user", () => {
		const named = tracker({ kind: "named", login: "jack" }, false);
		expect(decide(named.ref, "read", member, named.facts).ok).toBe(true);
		expect(decide(named.ref, "write", member, named.facts).ok).toBe(true);
		expect(decide(named.ref, "read", teammate, named.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("user:jack"),
		});
		expect(decide(named.ref, "write", teammate, named.facts).ok).toBe(false);
	});

	it("a per-user secret tracker: each user reads and writes only their own repo", () => {
		const own = tracker({ kind: "user" }, false, "jack");
		expect(decide(own.ref, "read", member, own.facts).ok).toBe(true);
		expect(decide(own.ref, "write", member, own.facts).ok).toBe(true);
		expect(decide(own.ref, "read", teammate, own.facts)).toMatchObject({ ok: false });
		expect(decide(own.ref, "write", teammate, own.facts)).toMatchObject({ ok: false });
		expect(decide(own.ref, "read", { login: "jack", member: false }, own.facts).ok).toBe(false);
		const shared = tracker({ kind: "user" }, false);
		expect(decide(shared.ref, "read", member, shared.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("per-user"),
		});
	});

	it("a tracker that is not per-user has no per-user repos", () => {
		const wrong = tracker({ kind: "project-devs" }, false, "jack");
		expect(decide(wrong.ref, "read", member, wrong.facts)).toMatchObject({ ok: false });
	});

	it("a fork: the tracker's audience reads, only the instance owner writes", () => {
		const devs = fork({ kind: "project-devs" }, true, "jack");
		expect(decide(devs.ref, "read", teammate, devs.facts).ok).toBe(true);
		expect(decide(devs.ref, "write", member, devs.facts).ok).toBe(true);
		expect(decide(devs.ref, "write", teammate, devs.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("belongs to jack"),
		});
		expect(decide(devs.ref, "read", outsider, devs.facts)).toMatchObject({ ok: false });
		const open = fork({ kind: "public" }, true, "jack");
		expect(decide(open.ref, "read", outsider, open.facts).ok).toBe(true);
		expect(decide(open.ref, "write", outsider, open.facts).ok).toBe(false);
	});

	it("a fork needs an instance, and only code trackers have forks", () => {
		const orphan = fork({ kind: "project-devs" }, true, undefined);
		expect(decide(orphan.ref, "read", member, orphan.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("no instance"),
		});
		const secret = fork({ kind: "project-devs" }, false, "jack");
		expect(decide(secret.ref, "read", member, secret.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("secret tracker"),
		});
	});

	it("a tracker the default branch does not define gets nothing", () => {
		for (const ref of [
			{ kind: "tracker", project: acme, tracker: "ghost" },
			{ kind: "tracker", project: acme, tracker: "ghost", login: "jack" },
			{ kind: "fork", project: acme, instance: "x", tracker: "ghost" },
		] as RepoRef[])
			expect(decide(ref, "read", member, { instanceOwner: "jack" })).toMatchObject({
				ok: false,
				reason: expect.stringContaining("default branch"),
			});
	});
});
