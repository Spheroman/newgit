import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS, signToken, verifyToken } from "../src/auth";

const KEY = "test-signing-key";
const login = (who: string, now?: number) =>
	signToken(KEY, { kind: "login", login: who, ttlSeconds: LOGIN_TTL_SECONDS }, now);

describe("newgit tokens", () => {
	it("verify to their claims, login lowercased, each with its own id", async () => {
		const first = await login("Jack");
		const second = await login("Jack");
		expect(first.token.startsWith("ngt_")).toBe(true);
		expect(await verifyToken(KEY, first.token)).toMatchObject({ sub: "jack", kind: "login", jti: first.claims.jti });
		expect(first.claims.jti).not.toBe(second.claims.jti);
	});

	it("carry an agent's name and project", async () => {
		const { token } = await signToken(KEY, {
			kind: "agent",
			login: "jack",
			agent: "agent-3",
			project: "acme",
			ttlSeconds: 3600,
		});
		expect(await verifyToken(KEY, token)).toMatchObject({ sub: "jack", kind: "agent", agent: "agent-3", project: "acme" });
	});

	it("are refused when signed with another key", async () => {
		const { token } = await signToken("other-key", { kind: "login", login: "jack", ttlSeconds: 60 });
		expect(await verifyToken(KEY, token)).toBeNull();
	});

	it("are refused when the payload is altered", async () => {
		const { token } = await login("jack");
		const forged = await login("eve");
		const [body] = forged.token.slice(4).split(".");
		const [, signature] = token.slice(4).split(".");
		expect(await verifyToken(KEY, `ngt_${body}.${signature}`)).toBeNull();
	});

	it("expire", async () => {
		const issued = Date.UTC(2026, 0, 1);
		const { token } = await login("jack", issued);
		expect(await verifyToken(KEY, token, issued + 29 * 86_400_000)).not.toBeNull();
		expect(await verifyToken(KEY, token, issued + 31 * 86_400_000)).toBeNull();
	});

	it("reject anything that is not one", async () => {
		for (const junk of ["", "ngt_", "ngt_a.b.c", "Bearer x", "art_v2_x"]) expect(await verifyToken(KEY, junk)).toBeNull();
	});
});
