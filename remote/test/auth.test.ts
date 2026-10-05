import { describe, expect, it } from "vitest";

import { issueToken, verifyToken } from "../src/auth";

const KEY = "test-signing-key";

describe("newgit tokens", () => {
	it("verify to the login they were issued to, lowercased", async () => {
		const token = await issueToken(KEY, "Jack");
		expect(token.startsWith("ngt_")).toBe(true);
		expect(await verifyToken(KEY, token)).toBe("jack");
	});

	it("are refused when signed with another key", async () => {
		const token = await issueToken("other-key", "jack");
		expect(await verifyToken(KEY, token)).toBeNull();
	});

	it("are refused when the payload is altered", async () => {
		const token = await issueToken(KEY, "jack");
		const forged = await issueToken(KEY, "eve");
		const [body] = forged.slice(4).split(".");
		const [, signature] = token.slice(4).split(".");
		expect(await verifyToken(KEY, `ngt_${body}.${signature}`)).toBeNull();
	});

	it("expire", async () => {
		const issued = Date.UTC(2026, 0, 1);
		const token = await issueToken(KEY, "jack", issued);
		expect(await verifyToken(KEY, token, issued + 29 * 86_400_000)).toBe("jack");
		expect(await verifyToken(KEY, token, issued + 31 * 86_400_000)).toBeNull();
	});

	it("reject anything that is not one", async () => {
		for (const junk of ["", "ngt_", "ngt_a.b.c", "Bearer x", "art_v2_x"]) expect(await verifyToken(KEY, junk)).toBeNull();
	});
});
