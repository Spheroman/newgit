import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { type UsageRow, checkBudget, fetchUsage, weeklySpend } from "../src/budget";
import { handle, issue } from "../src/index";
import { FakeRepos, noEndpoint } from "./fake-repos";

const NOW = new Date("2026-11-02T15:00:00Z");
const row = (day: string, cost: number): UsageRow => ({ ChargePeriodStart: `${day}T00:00:00Z`, ContractedCost: cost });

/** Cloudflare's billable-usage endpoint, answering with `rows`, and the URLs it was asked for. */
function billing(rows: UsageRow[], status = 200) {
	const asked: string[] = [];
	const fetcher = (async (input: RequestInfo | URL) => {
		asked.push(String(input));
		return Response.json(
			status === 200 ? { success: true, result: rows, errors: [], messages: [] } : { success: false, errors: [{ message: "nope" }] },
			{ status },
		);
	}) as typeof fetch;
	return { fetcher, asked };
}

const settings = (extra: Partial<Env> = {}): Env => ({ ...env, BILLING_TOKEN: "billing", ADMIN_LOGINS: "op", ...extra });

async function me(e: Env, who: string) {
	const token = (await issue(e, { kind: "login", login: who, ttlSeconds: LOGIN_TTL_SECONDS })).token;
	const response = await handle(
		new Request("https://newgit.dev/v1/me", { headers: { authorization: `Bearer ${token}` } }),
		e,
		{ repos: new FakeRepos(), fetcher: fetch, waitUntil: () => {}, endpoint: noEndpoint },
	);
	return { status: response.status, body: (await response.json()) as { error?: string } };
}

describe("weekly spend", () => {
	it("counts today and the 6 days before it, and nothing older", () => {
		const rows = [row("2026-10-26", 100), row("2026-10-27", 1.25), row("2026-11-01", 2), row("2026-11-02", 0.5), { ...row("2026-11-02", 0), ContractedCost: null }];
		expect(weeklySpend(rows, NOW)).toBe(3.75);
	});

	it("asks for a range reaching back past the start of the billing period", async () => {
		const { fetcher, asked } = billing([]);
		await fetchUsage("acct", "t", NOW, fetcher);
		expect(asked).toEqual(["https://api.cloudflare.com/client/v4/accounts/acct/billable-usage?from=2026-09-17&to=2026-11-03"]);
	});

	it("fails loudly when Cloudflare refuses", async () => {
		await expect(fetchUsage("acct", "t", NOW, billing([], 403).fetcher)).rejects.toThrow("nope");
	});
});

describe("the kill switch", () => {
	it("pauses the remote past the budget for everyone but admins, and reopens under it", async () => {
		const e = settings();
		const over = await checkBudget(e, billing([row("2026-11-01", 6), row("2026-11-02", 4.5)]).fetcher, NOW);
		expect(over).toMatchObject({ spent: 10.5, limit: 10, paused: true });

		const refused = await me(e, "stranger");
		expect(refused.status).toBe(503);
		expect(refused.body.error).toContain("billed $10.50 in the last 7 days, over its $10 weekly budget");
		expect((await me(e, "op")).status).toBe(200);

		expect(await checkBudget(e, billing([row("2026-11-02", 9.99)]).fetcher, NOW)).toMatchObject({ paused: false });
		expect((await me(e, "stranger")).status).toBe(200);
	});

	it("reads SPEND_LIMIT_WEEKLY_USD", async () => {
		const e = settings({ SPEND_LIMIT_WEEKLY_USD: "25" });
		expect(await checkBudget(e, billing([row("2026-11-02", 20)]).fetcher, NOW)).toMatchObject({ limit: 25, paused: false });
	});

	it("does nothing without a billing token", async () => {
		const { fetcher, asked } = billing([row("2026-11-02", 1000)]);
		expect(await checkBudget(settings({ BILLING_TOKEN: undefined }), fetcher, NOW)).toBeNull();
		expect(asked).toEqual([]);
		expect((await me(settings(), "stranger")).status).toBe(200);
	});

	it("keeps the last state when the read fails", async () => {
		const e = settings();
		await expect(checkBudget(e, billing([], 500).fetcher, NOW)).rejects.toThrow();
		expect((await me(e, "stranger")).status).toBe(200);
	});
});
