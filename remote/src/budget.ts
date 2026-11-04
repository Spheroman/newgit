// The spend kill switch. Cloudflare has no hard spending cap, so the remote
// keeps its own: an hourly cron reads what the account was billed over the
// last 7 days (the Billable Usage API) and, past the weekly budget, pauses
// the remote for everyone but its admins. It reopens on its own once the
// last 7 days fall back under the budget.
//
// The numbers are Cloudflare's, so they lag: usage is processed daily, and
// the switch trips at most a day or so after the line is crossed.

import { DurableObject } from "cloudflare:workers";

/** The budget when `SPEND_LIMIT_WEEKLY_USD` is unset. */
export const DEFAULT_WEEKLY_LIMIT = 10;

const DAY_MS = 86_400_000;

export interface BudgetState {
	/** What the last 7 days cost, in the account's billing currency. */
	spent: number;
	limit: number;
	paused: boolean;
	/** When the cron last read Cloudflare's numbers. */
	checkedAt: string;
}

/** One row of `GET /accounts/{id}/billable-usage`: one product, one day. */
export interface UsageRow {
	ChargePeriodStart: string;
	ContractedCost: number | null;
	BillingCurrency?: string;
	ServiceName?: string;
}

/** The single record of the remote's spend: one instance, named "budget". */
export class BudgetDO extends DurableObject<Env> {
	private readonly sql: SqlStorage;

	constructor(ctx: DurableObjectState, env: Env) {
		super(ctx, env);
		this.sql = ctx.storage.sql;
		this.sql.exec(`
			CREATE TABLE IF NOT EXISTS budget (
				id INTEGER PRIMARY KEY CHECK (id = 1),
				spent REAL NOT NULL, spend_limit REAL NOT NULL, paused INTEGER NOT NULL, checked_at TEXT NOT NULL);
		`);
	}

	record(state: BudgetState): void {
		this.sql.exec(
			`INSERT INTO budget (id, spent, spend_limit, paused, checked_at) VALUES (1, ?, ?, ?, ?)
			 ON CONFLICT (id) DO UPDATE SET spent = excluded.spent, spend_limit = excluded.spend_limit,
			   paused = excluded.paused, checked_at = excluded.checked_at`,
			state.spent,
			state.limit,
			state.paused ? 1 : 0,
			state.checkedAt,
		);
	}

	state(): BudgetState | null {
		const row = this.sql
			.exec<{ spent: number; spend_limit: number; paused: number; checked_at: string }>("SELECT * FROM budget")
			.toArray()[0];
		if (!row) return null;
		return { spent: row.spent, limit: row.spend_limit, paused: row.paused === 1, checkedAt: row.checked_at };
	}
}

/** The weekly budget: `SPEND_LIMIT_WEEKLY_USD`, or $10. */
export function weeklyLimit(env: Env): number {
	const limit = Number.parseFloat(env.SPEND_LIMIT_WEEKLY_USD ?? "");
	return Number.isFinite(limit) && limit >= 0 ? limit : DEFAULT_WEEKLY_LIMIT;
}

/** The start of the 7-day window ending today (UTC): today and the 6 days before it. */
function windowStart(now: Date): number {
	const today = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate());
	return today - 6 * DAY_MS;
}

/** What the rows charged within the 7-day window ending today. */
export function weeklySpend(rows: UsageRow[], now: Date): number {
	const start = windowStart(now);
	let spent = 0;
	for (const row of rows) if (Date.parse(row.ChargePeriodStart) >= start) spent += row.ContractedCost ?? 0;
	return Math.round(spent * 100) / 100;
}

const ymd = (ms: number) => new Date(ms).toISOString().slice(0, 10);

/**
 * Every usage row that could fall in the window. The API selects rows by
 * billing period, and returns nothing for a range that starts after the
 * period does, so `from` reaches back past the start of any monthly period
 * the window could touch; `weeklySpend` keeps only the window's days.
 */
export async function fetchUsage(accountId: string, token: string, now: Date, fetcher: typeof fetch): Promise<UsageRow[]> {
	const from = ymd(windowStart(now) - 40 * DAY_MS);
	const to = ymd(now.getTime() + DAY_MS);
	const response = await fetcher(
		`https://api.cloudflare.com/client/v4/accounts/${accountId}/billable-usage?from=${from}&to=${to}`,
		{ headers: { authorization: `Bearer ${token}` } },
	);
	const body = (await response.json().catch(() => null)) as
		| { success?: boolean; result?: UsageRow[]; errors?: { message: string }[] }
		| null;
	if (!response.ok || !body?.success || !Array.isArray(body.result))
		throw new Error(
			`billable-usage answered ${response.status}: ${body?.errors?.map((e) => e.message).join("; ") ?? "no JSON"}`,
		);
	return body.result;
}

const stub = (env: Env) => env.BUDGET.getByName("budget");

/** Each isolate's last look at the state, so requests do not all call one Durable Object. */
let cached: { state: BudgetState | null; until: number } | null = null;
const CACHE_MS = 30_000;

/**
 * Read Cloudflare's numbers and record whether the remote is over budget.
 * Without `BILLING_TOKEN` there is nothing to read, and nothing changes. A
 * failed read changes nothing either: the last state stands.
 */
export async function checkBudget(env: Env, fetcher: typeof fetch, now = new Date()): Promise<BudgetState | null> {
	if (!env.BILLING_TOKEN) {
		console.warn("budget: BILLING_TOKEN is not set, so the weekly budget is not enforced");
		return null;
	}
	const rows = await fetchUsage(env.CLOUDFLARE_ACCOUNT_ID, env.BILLING_TOKEN, now, fetcher);
	const limit = weeklyLimit(env);
	const spent = weeklySpend(rows, now);
	const state: BudgetState = { spent, limit, paused: spent > limit, checkedAt: now.toISOString() };
	const before = await stub(env).state();
	await stub(env).record(state);
	cached = { state, until: Date.now() + CACHE_MS };
	if (state.paused !== (before?.paused ?? false))
		console.warn(
			state.paused
				? `budget: PAUSED — $${spent} spent in the last 7 days, over the $${limit} budget`
				: `budget: reopened — $${spent} spent in the last 7 days, under the $${limit} budget`,
		);
	else console.log(`budget: $${spent} of $${limit} spent in the last 7 days`);
	return state;
}

/** The budget's state, if the remote is paused; null while it is open. */
export async function pausedBy(env: Env): Promise<BudgetState | null> {
	if (cached === null || cached.until < Date.now())
		cached = { state: await stub(env).state(), until: Date.now() + CACHE_MS };
	return cached.state?.paused ? cached.state : null;
}
