import { env } from "cloudflare:workers";
import { beforeEach, expect } from "vitest";

import { LOGIN_TTL_SECONDS } from "../src/auth";
import { CHECKING, type PullAnswer, type PullPlan } from "../src/endpoint";
import { type Deps, handle, issue } from "../src/index";
import { advancePulls } from "../src/webhook";
import { FakeRepos, noEndpoint } from "./fake-repos";

// What the webhook's tests share: a project mirroring to Spheroman/newgit with
// `t3code` withheld, a faked container, and signed deliveries.

export const SECRET = "shh, webhook";
export const secretEnv = new Proxy(env, {
	get: (target, key) => (key === "GITHUB_WEBHOOK_SECRET" ? SECRET : Reflect.get(target, key)),
}) as Env;

export const sha = (c: string) => c.repeat(40);
export const main = { root: sha("a"), access: sha("d"), t3code: sha("e") };
export const THEIRS = sha("b");

export let repos: FakeRepos;
export let name: string;
export let jack: string;
type Answer = PullAnswer | typeof CHECKING | Error;
/** What the fake container does next: for a landing, and for a look. */
let answers: Answer[];
let looks: Answer[];
export const setAnswers = (next: Answer[]) => {
	answers = next;
};
export const setLooks = (next: Answer[]) => {
	looks = next;
};
/** The plans the container was asked to land, and to look at. */
export let plans: PullPlan[];
export let peeks: PullPlan[];
let started: PullPlan["repos"][];

const pull: Deps["pull"] = async (_project, plan, credentials) => {
	for (const repo of Object.values(plan.repos)) expect(repo.auth, "an ask carries no passwords").toBe("");
	if (plan.peek) {
		peeks.push(plan);
		await credentials();
		const look = looks.shift() ?? { outcome: "work" };
		if (look instanceof Error) throw look;
		return look as PullAnswer;
	}
	plans.push(plan);
	const answer = answers.shift() ?? { outcome: "nothing" };
	if (started.length < plans.length) started.push(await credentials());
	if (answer instanceof Error) throw answer;
	return answer as PullAnswer;
};

export const deps = (): Deps => ({
	repos,
	fetcher: fetch,
	waitUntil: () => {},
	endpoint: noEndpoint,
	pull,
	ancestry: async (_project, plan) =>
		plan.checks.map((check) => ({ tracker: check.tracker, forward: true, onMain: true })),
	index: async (_project, plan) => ({ matches: true, actual: plan.list }),
});

export async function call(token: string, method: string, route: string, body?: unknown) {
	const response = await handle(
		new Request(`https://newgit.dev${route}`, {
			method,
			headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
			body: body === undefined ? undefined : JSON.stringify(body),
		}),
		secretEnv,
		deps(),
	);
	return { status: response.status, body: (await response.json()) as any };
}

export const path = (rest: string) => `/v1/projects/jack/${name}${rest}`;
export const stub = () => env.PROJECTS.getByName(`jack/${name}`);
export const index = async () => (await call(jack, "GET", path("/branches/main"))).body.list;

export async function sign(secret: string, body: string) {
	const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
		"sign",
	]);
	const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(body)));
	return `sha256=${[...mac].map((byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

export const push = (overrides: Record<string, unknown> = {}) => ({
	ref: "refs/heads/main",
	after: THEIRS,
	repository: { full_name: "Spheroman/newgit", private: false },
	pusher: { name: "jack" },
	...overrides,
});

interface Delivery {
	event?: string;
	payload?: unknown;
	signature?: string | null;
	query?: string;
	useEnv?: Env;
	/** Leave the queue for the alarm. Otherwise one step of it runs, as the alarm would. */
	hold?: boolean;
}

export async function deliver({ event = "push", payload = push(), signature, query, useEnv = secretEnv, hold = false }: Delivery = {}) {
	const body = JSON.stringify(payload);
	const headers = new Headers({ "x-github-event": event });
	const signed = signature === undefined ? await sign(SECRET, body) : signature;
	if (signed !== null) headers.set("x-hub-signature-256", signed);
	const response = await handle(
		new Request(`https://newgit.dev/v1/webhooks/github?${query ?? `project=jack/${name}&target=github`}`, {
			method: "POST",
			headers,
			body,
		}),
		useEnv,
		deps(),
	);
	if (response.status === 202 && !hold) await advance();
	return { status: response.status, body: (await response.json()) as any };
}

export const advance = () => advancePulls(secretEnv, deps(), { owner: "jack", name });

beforeEach(async () => {
	repos = new FakeRepos();
	answers = [];
	looks = [];
	plans = [];
	peeks = [];
	started = [];
	name = `hook-${crypto.randomUUID().slice(0, 8)}`;
	jack = (await issue(env, { kind: "login", login: "jack", ttlSeconds: LOGIN_TTL_SECONDS })).token;
	await call(jack, "POST", "/v1/projects", { name, trackers: ["root", "access", "t3code"] });
	repos.write(`jack.${name}.t.access`, "root.toml", 'audience = "project-devs"\ngit = true\n');
	repos.write(
		`jack.${name}.t.access`,
		"t3code.toml",
		'audience = "public"\ngit = true\nmount = "apps/t3code"\npaths = ["."]\n',
	);
	repos.write(
		`jack.${name}.t.root`,
		".newgit/exports/github.toml",
		'url = "Spheroman/newgit"\ntrackers = ["root", "access"]\n',
	);
	repos.write(`jack.${name}.t.root`, ".newgit/exports/t3code.toml", 'url = "Spheroman/t3code"\nroot = "t3code"\n');
	await call(jack, "PUT", path("/branches/main"), { list: main });
});

