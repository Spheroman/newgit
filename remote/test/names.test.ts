import { describe, expect, it } from "vitest";

import { type Audience, type RepoRef, decide, parseAudience, parseRepoName, repoName } from "../src/names";

const member = { login: "jack", member: true };
const teammate = { login: "ana", member: true };
const outsider = { login: "eve", member: false };

describe("repo names", () => {
	const refs: RepoRef[] = [
		{ kind: "src", project: "acme" },
		{ kind: "instance", project: "acme", instance: "fix-login" },
		{ kind: "tracker", project: "acme", tracker: "runtime-env" },
		{ kind: "tracker", project: "acme", tracker: "runtime-env", login: "jack" },
	];

	it("round-trips every kind", () => {
		for (const ref of refs) expect(parseRepoName(repoName(ref))).toEqual(ref);
	});

	it("refuses names that are not one of a project's repos", () => {
		for (const name of ["acme", "acme.x", "acme.i", "acme.t.a.b.c", "acme..src", ".src", "acme.src.extra"])
			expect(parseRepoName(name), name).toBeNull();
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
	const src: RepoRef = { kind: "src", project: "acme" };
	const fork: RepoRef = { kind: "instance", project: "acme", instance: "fix-login" };
	const tracker = (audience: Audience, login?: string) => ({
		ref: { kind: "tracker", project: "acme", tracker: "env", login } as RepoRef,
		facts: { audience },
	});

	it("trunk: members read, nobody writes", () => {
		expect(decide(src, "read", member, {}).ok).toBe(true);
		expect(decide(src, "read", outsider, {}).ok).toBe(false);
		const write = decide(src, "write", member, {});
		expect(write).toMatchObject({ ok: false, reason: expect.stringContaining("newgit merge") });
	});

	it("an instance fork: members read, only its owner writes", () => {
		const facts = { instanceOwner: "jack" };
		expect(decide(fork, "read", teammate, facts).ok).toBe(true);
		expect(decide(fork, "write", member, facts).ok).toBe(true);
		expect(decide(fork, "write", teammate, facts)).toMatchObject({ ok: false });
		expect(decide(fork, "read", outsider, facts)).toMatchObject({ ok: false });
		expect(decide(fork, "read", member, {})).toMatchObject({ ok: false, reason: expect.stringContaining("no instance") });
	});

	it("a per-user tracker: each user reads and writes only their own repo", () => {
		const own = tracker({ kind: "user" }, "jack");
		expect(decide(own.ref, "read", member, own.facts).ok).toBe(true);
		expect(decide(own.ref, "write", member, own.facts).ok).toBe(true);
		expect(decide(own.ref, "read", teammate, own.facts)).toMatchObject({ ok: false });
		const shared = tracker({ kind: "user" });
		expect(decide(shared.ref, "read", member, shared.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("per-user"),
		});
	});

	it("a named tracker: only that user", () => {
		const named = tracker({ kind: "named", login: "jack" });
		expect(decide(named.ref, "read", member, named.facts).ok).toBe(true);
		expect(decide(named.ref, "read", teammate, named.facts)).toMatchObject({
			ok: false,
			reason: expect.stringContaining("user:jack"),
		});
	});

	it("project-devs: members; public: anyone reads, members write", () => {
		const devs = tracker({ kind: "project-devs" });
		expect(decide(devs.ref, "write", teammate, devs.facts).ok).toBe(true);
		expect(decide(devs.ref, "read", outsider, devs.facts).ok).toBe(false);
		const open = tracker({ kind: "public" });
		expect(decide(open.ref, "read", outsider, open.facts).ok).toBe(true);
		expect(decide(open.ref, "write", outsider, open.facts).ok).toBe(false);
	});

	it("a tracker trunk does not define gets nothing", () => {
		const ref: RepoRef = { kind: "tracker", project: "acme", tracker: "ghost" };
		expect(decide(ref, "read", member, {})).toMatchObject({ ok: false, reason: expect.stringContaining("trunk") });
	});

	it("a shared tracker has no per-user repos", () => {
		const wrong = tracker({ kind: "project-devs" }, "jack");
		expect(decide(wrong.ref, "read", member, wrong.facts)).toMatchObject({ ok: false });
	});
});
