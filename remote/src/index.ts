// The newgit remote's API. Every route but login takes `Authorization: Bearer
// ngt_…` and answers JSON; errors are `{ error }` with a status that says
// whose problem it is. Built for the CLI (and so for agents): the web UI uses
// the same routes.

import { parse as parseToml } from "smol-toml";

// The one-line installer, bundled as text (the `rules` in wrangler.jsonc).
import installScript from "../../scripts/install.sh";

import { type Repos, ArtifactsRepos } from "./artifacts";
import {
	AGENT_TTL_MAX_SECONDS,
	AGENT_TTL_SECONDS,
	LOGIN_TTL_SECONDS,
	listed,
	type NewToken,
	pollDeviceFlow,
	signToken,
	startDeviceFlow,
	verifyToken,
} from "./auth";
import {
	type Audience,
	type Facts,
	type ProjectId,
	type RepoRef,
	decide,
	parseAudience,
	parseMaintainers,
	mayMaintain,
	type Maintainers,
	parseProjectKey,
	parseRepoName,
	partProblem,
	projectKey,
	repoName,
} from "./names";
import { checkBudget, pausedBy } from "./budget";
import {
	type AncestryAnswer,
	type AncestryPlan,
	type BackgroundKind,
	CHECKING,
	type CopyAnswer,
	type CopyPlan,
	type EndpointKind,
	type EndpointPlan,
	type IndexAnswer,
	type IndexPlan,
	PLAN_HEADER,
	type PullAnswer,
	type PullPlan,
} from "./endpoint";
import { githubWebhook } from "./webhook";

type Checking = typeof CHECKING;
import type { Branch, Instance, ProjectDO, TrackerList } from "./project";
import { type RefMove, followLanding } from "./follow";
import { ASK_GIT, nativeAncestry, nativeIndex } from "./walk";
import type { UserDO } from "./users";

export { BudgetDO } from "./budget";
export { GitEndpoint } from "./endpoint";
export { ProjectDO } from "./project";
export { UserDO } from "./users";

/** Who a request acts as, after the token was verified and found unrevoked. */
export interface Caller {
	login: string;
	tokenId: string;
	kind: "login" | "agent";
	/** Set for agent tokens. */
	agent?: string;
	/** Set for agent tokens pinned to one project. */
	project?: string;
	/** Set for instance tokens: the one instance this caller may write. */
	instance?: string;
}

/** Git credentials last this long: long enough for a big push, short enough that losing membership bites fast. */
export const CREDENTIAL_TTL_SECONDS = 15 * 60;

/**
 * Read credentials last this long: a stepped fetch of a large history
 * (t3code's 365 MiB) outlasts fifteen minutes. Each is recorded and revoked
 * with the login or instance it was minted for, so losing membership still
 * bites at once.
 */
export const READ_CREDENTIAL_TTL_SECONDS = 60 * 60;

/**
 * The read passwords a merge check fetches with: as long as the check may
 * run (`CHECK_LIMIT_MS`, 45 minutes) and a little more, since a fetch of a
 * large history starts minutes in and must not lose its password part way.
 */
const CHECK_CREDENTIAL_SECONDS = 50 * 60;

/**
 * The passwords a fork's copy fetches and pushes with: as long as the copy
 * may run (`COPY_LIMIT_MS`, two hours) and a little more, so a push of a
 * large history late in the copy does not lose its password.
 */
const COPY_CREDENTIAL_SECONDS = 130 * 60;

/**
 * How soon a client should ask again while the remote copies a fork. Short:
 * a small tracker's copy is done in seconds, and every fork now waits at
 * least one of these.
 */
const FORK_RETRY_SECONDS = 5;

/** How soon a client should ask again while the remote checks its merge. */
const CHECK_RETRY_SECONDS = 10;

/** The answer while the endpoint is still checking a merge. */
const stillChecking = () =>
	Response.json(CHECKING, { status: 202, headers: { "retry-after": String(CHECK_RETRY_SECONDS) } });

/** Whether a check's answer is that it is still running. */
export function isChecking(answer: unknown): answer is Checking {
	return typeof answer === "object" && answer !== null && (answer as { checking?: unknown }).checking === true;
}

/** A merge's write passwords last this long: pushing the trackers' commits to landing refs. */
const MERGE_PUSH_SECONDS = 10 * 60;
/**
 * A merge begun is remembered this long: its pushes, then the remote's
 * check of its index (the first time a large history is there, many
 * minutes), past the hour the client keeps asking, and after it lands, for
 * a finish asked again. Nothing waits on it; it only bounds how long its
 * record lives.
 */
const MERGE_PENDING_SECONDS = 70 * 60;

export interface Deps {
	repos: Repos;
	/** Start each new instance's fork of root when it is created (live: yes). */
	prefork?: boolean;
	fetcher: typeof fetch;
	/** Work that may outlive the response. */
	waitUntil(promise: Promise<unknown>): void;
	/** Serve one Git request from a project's endpoint container. */
	endpoint(project: string, kind: EndpointKind, plan: EndpointPlan, request: Request): Promise<Response>;
	/**
	 * Ask the project's endpoint container, which runs Git, how each pin
	 * moves along its tracker's history. Absent (tests that do not fake it),
	 * every move counts as not forward.
	 *
	 * Both checks run in the background (src/endpoint.ts): the answer is
	 * `CHECKING` until the check ends, and asking again with the same plan
	 * collects it. `plan`'s repos carry no credentials; `credentials` mints
	 * them, and is called only when the check has to start.
	 */
	ancestry?(
		project: string,
		plan: AncestryPlan,
		credentials: () => Promise<AncestryPlan["repos"]>,
	): Promise<AncestryAnswer[] | Checking>;
	/**
	 * Ask the container whether an index is what its root commit pins. The
	 * production Deps always has it; a test that does not fake it skips the
	 * check.
	 */
	index?(
		project: string,
		plan: IndexPlan,
		credentials: () => Promise<IndexPlan["repos"]>,
	): Promise<IndexAnswer | Checking>;
	/**
	 * Have the container copy every branch of a tracker's repo into an
	 * instance's empty fork, in the background, as the checks run. Absent
	 * (tests that do not fake it), the fork route answers 501.
	 */
	copy?(
		project: string,
		plan: CopyPlan,
		credentials: () => Promise<Pick<CopyPlan, "from" | "to">>,
	): Promise<CopyAnswer | Checking>;
	/**
	 * Have the container bring a mirror's commit into `main` and land it, in
	 * the background, as the checks run. Absent, a webhook's pull fails.
	 */
	pull?(
		project: string,
		plan: PullPlan,
		credentials: () => Promise<PullPlan["repos"]>,
	): Promise<PullAnswer | Checking>;
}

/**
 * Ask the endpoint after a merge check: a peek, without credentials, at one
 * already started; and only when there is none, the plan with credentials,
 * which starts it.
 */
async function askCheck<P>(
	env: Env,
	project: string,
	kind: BackgroundKind,
	plan: P,
	credentials: () => Promise<Partial<P>>,
	failure: string,
): Promise<unknown> {
	const endpoint = env.ENDPOINTS.getByName(project);
	const ask = (sent: P, peek: boolean) =>
		endpoint.fetch(
			new Request(`https://endpoint/${kind}${peek ? "?peek" : ""}`, {
				method: "POST",
				headers: { [PLAN_HEADER]: btoa(JSON.stringify(sent)) },
			}),
		);
	let response = await ask(plan, true);
	if (response.status === 404) {
		await response.body?.cancel();
		response = await ask({ ...plan, ...(await credentials()) }, false);
	}
	if (response.status === 202) {
		await response.body?.cancel();
		return CHECKING;
	}
	if (!response.ok) throw new HttpError(502, `${failure}: ${(await response.text()).trim()}`);
	return response.json();
}

export class HttpError extends Error {
	constructor(
		readonly status: number,
		message: string,
	) {
		super(message);
	}
}

const json = (body: unknown, status = 200) => Response.json(body, { status });

/** The real thing: what the Worker runs on, and `ProjectDO`'s alarm. */
export function liveDeps(env: Env, waitUntil: Deps["waitUntil"]): Deps {
	return {
		repos: new ArtifactsRepos(env.ARTIFACTS, env.ARTIFACTS_GIT_BASE),
		fetcher: fetch,
		waitUntil,
		prefork: true,
		ancestry: async (project, plan, credentials) =>
			(await askCheck(
				env,
				project,
				"ancestry",
				plan,
				async () => ({ repos: await credentials() }),
				"the remote could not check where the merge's pins move",
			)) as AncestryAnswer[] | Checking,
		index: async (project, plan, credentials) =>
			(await askCheck(
				env,
				project,
				"index",
				plan,
				async () => ({ repos: await credentials() }),
				"the remote could not check the merge's index",
			)) as IndexAnswer | Checking,
		copy: async (project, plan, credentials) =>
			(await askCheck(env, project, "copy", plan, credentials, "its Git endpoint said")) as CopyAnswer | Checking,
		pull: async (project, plan, credentials) =>
			(await askCheck(
				env,
				project,
				"pull",
				plan,
				async () => ({ repos: await credentials() }),
				"the Git endpoint could not pull",
			)) as PullAnswer | Checking,
		endpoint: (project, kind, plan, original) => {
			const headers = new Headers({ [PLAN_HEADER]: btoa(JSON.stringify(plan)) });
			for (const name of ["git-protocol", "content-encoding", "content-type"]) {
				const value = original.headers.get(name);
				if (value) headers.set(name, value);
			}
			return env.ENDPOINTS.getByName(project).fetch(
				new Request(`https://endpoint/${kind}`, { method: "POST", headers, body: original.body }),
			);
		},
	};
}

export default {
	async fetch(request, env, ctx): Promise<Response> {
		// The landing page is read from a tracker on every miss, so the edge
		// keeps it for as long as it says it may (`landingPage`).
		const landing = request.method === "GET" && new URL(request.url).pathname === "/";
		if (landing) {
			const cached = await caches.default.match(request);
			if (cached) return cached;
		}
		const response = await handle(request, env, liveDeps(env, (promise) => ctx.waitUntil(promise)));
		if (landing && response.status === 200) ctx.waitUntil(caches.default.put(request, response.clone()));
		return response;
	},

	async scheduled(_controller, env, ctx) {
		ctx.waitUntil(checkBudget(env, fetch));
	},
} satisfies ExportedHandler<Env>;

export async function handle(request: Request, env: Env, deps: Deps): Promise<Response> {
	const git = new URL(request.url).pathname.match(GIT_ROUTE);
	try {
		if (git) return await gitEndpoint(request, env, deps, git);
		return await route(request, env, deps);
	} catch (error) {
		// Git shows a plain-text body to whoever ran it; JSON is for the API.
		if (error instanceof HttpError && git)
			return new Response(`newgit: ${error.message}\n`, {
				status: error.status,
				headers:
					error.status === 401 ? { "www-authenticate": 'Basic realm="newgit"' } : { "content-type": "text/plain" },
			});
		if (error instanceof HttpError) return json({ error: error.message }, error.status);
		// A bare error logs only its message; an ArtifactsError's code and the
		// stack are what say which call failed and why.
		const { name, code, message, stack } = error as { name?: string; code?: string; message?: string; stack?: string };
		console.error(JSON.stringify({ path: new URL(request.url).pathname, name, code, message, stack }));
		return json({ error: "internal error" }, 500);
	}
}

async function route(request: Request, env: Env, deps: Deps): Promise<Response> {
	const url = new URL(request.url);
	const path = url.pathname.replace(/\/+$/, "");
	const method = request.method;

	// ── Install (`curl -fsSL https://newgit.dev/install.sh | sh`) ──────────
	if ((method === "GET" || method === "HEAD") && (path === "/install.sh" || path === "/install"))
		return new Response(method === "HEAD" ? null : installScript, {
			headers: { "content-type": "text/x-shellscript; charset=utf-8", "cache-control": "public, max-age=300" },
		});

	// ── The landing page ───────────────────────────────────────────────────
	if ((method === "GET" || method === "HEAD") && path === "") return landingPage(method, env, deps.repos);

	// ── GitHub's webhook: signed, not logged in ────────────────────────────
	if (method === "POST" && path === "/v1/webhooks/github") return githubWebhook(request, env, deps);

	// ── Login (no token yet) ───────────────────────────────────────────────
	if (method === "POST" && path === "/v1/login/device") {
		if (!env.GITHUB_CLIENT_ID) throw new HttpError(503, "this remote has no GitHub app configured");
		return json(await startDeviceFlow(env.GITHUB_CLIENT_ID, deps.fetcher));
	}
	if (method === "POST" && path === "/v1/login/poll") {
		const { device_code } = await body<{ device_code?: string }>(request);
		if (!device_code) throw new HttpError(400, "device_code is required");
		const result = await pollDeviceFlow(env.GITHUB_CLIENT_ID, device_code, deps.fetcher);
		if (result.status !== "done") return json(result, result.status === "pending" ? 202 : 401);
		await requireInvited(env, result.login);
		const issued = await issue(env, { kind: "login", login: result.login, ttlSeconds: LOGIN_TTL_SECONDS });
		return json({ status: "done", login: result.login, token: issued.token, token_id: issued.id });
	}

	const caller = await authenticate(request, env);
	const login = caller.login;

	if (method === "GET" && path === "/v1/me")
		return json({ login, kind: caller.kind, token_id: caller.tokenId, agent: caller.agent, project: caller.project });

	// ── Tokens ─────────────────────────────────────────────────────────────
	if (method === "GET" && path === "/v1/tokens") {
		requireHuman(caller, "list tokens");
		return json({ tokens: await userStub(env, login).tokens() });
	}
	if (method === "POST" && path === "/v1/tokens") {
		// An agent cannot mint tokens, so a leaked one cannot outlive its revocation.
		requireHuman(caller, "create tokens");
		const { name, project, ttl_days } = await body<{ name?: string; project?: string; ttl_days?: number }>(request);
		if (!name) throw new HttpError(400, "name is required: what the agent is called in records and the UI");
		requirePart("agent name", name);
		if (project !== undefined && parseProjectKey(project) === null)
			throw new HttpError(400, "project is <owner>/<name>");
		const ttlSeconds = ttl_days === undefined ? AGENT_TTL_SECONDS : Math.round(ttl_days * 86_400);
		if (!(ttlSeconds > 0 && ttlSeconds <= AGENT_TTL_MAX_SECONDS))
			throw new HttpError(400, `ttl_days must be more than 0 and at most ${AGENT_TTL_MAX_SECONDS / 86_400}`);
		const issued = await issue(env, { kind: "agent", login, agent: name, project, ttlSeconds });
		return json({ token: issued.token, ...issued.record }, 201);
	}
	if (method === "POST" && path === "/v1/tokens/renew") {
		// An agent or instance token replaces itself before it expires: a fresh
		// token with the same reach, and the old one revoked — so a copy stolen
		// earlier stops working at the next renewal.
		if (caller.kind !== "agent") throw new HttpError(400, "a login renews by running `newgit login`");
		if (caller.instance !== undefined && caller.project !== undefined) {
			const pinned = parseProjectKey(caller.project);
			const alive = pinned === null ? null : await projectStub(env, pinned).instance(caller.instance);
			if (alive === null) throw new HttpError(410, `instance \`${caller.instance}\` was removed`);
		}
		const issued = await issue(env, {
			kind: "agent",
			login,
			agent: caller.agent,
			project: caller.project,
			instance: caller.instance,
			ttlSeconds: AGENT_TTL_SECONDS,
		});
		await userStub(env, login).revoke(caller.tokenId, true);
		return json({ token: issued.token, ...issued.record }, 201);
	}
	const tokenMatch = path.match(/^\/v1\/tokens\/([^/]+)$/);
	if (method === "DELETE" && tokenMatch) {
		const id = decodeURIComponent(tokenMatch[1]);
		// A human revokes any of their tokens; an agent may revoke only itself.
		if (caller.kind === "agent" && id !== caller.tokenId) requireHuman(caller, "revoke other tokens");
		const { revoked, repoTokens } = await userStub(env, login).revoke(id);
		if (!revoked) throw new HttpError(404, `no token \`${id}\``);
		await revokeRepoTokens(deps, repoTokens);
		return json({ id, revoked: true, repo_tokens_revoked: repoTokens.length });
	}

	// ── Projects ───────────────────────────────────────────────────────────
	if (method === "POST" && path === "/v1/projects") {
		requireHuman(caller, "create projects");
		const { name, trackers = [] } = await body<{ name?: string; trackers?: string[] }>(request);
		if (!name) throw new HttpError(400, "name is required");
		requirePart("project name", name);
		if (!Array.isArray(trackers) || trackers.length === 0)
			throw new HttpError(400, "trackers is required: the trackers in Git whose repos to create (root, access, …)");
		for (const tracker of trackers) requirePart("tracker name", tracker);
		const id: ProjectId = { owner: login, name };
		const key = projectKey(id);
		const project = projectStub(env, id);
		const owner = userStub(env, login);
		if (!listed(env.ADMIN_LOGINS, login)) {
			const cap = projectCap(env);
			if (!(await owner.claimProject(key, cap)))
				throw new HttpError(
					403,
					`${login} already owns ${cap} projects, the most one login may on this remote: delete one with \`newgit remote delete\``,
				);
		}
		if (!(await project.init(name, login))) throw new HttpError(409, `project \`${key}\` already exists`);
		// The one write to the repo of a tracker in Git outside a merge: the creator's
		// initial push of its history. Only here, only once, only briefly.
		const seeds = [];
		for (const tracker of trackers) seeds.push(await seed(env, deps, caller, { kind: "tracker", project: id, tracker }));
		return json(
			{ owner: login, name, key, endpoint: endpointUrl(url, id), seeds },
			201,
		);
	}

	const projectMatch = path.match(/^\/v1\/projects\/([^/]+)\/([^/]+)(\/.*)?$/);
	if (!projectMatch) throw new HttpError(404, `no route for ${method} ${path}`);
	const id = parseProjectKey(`${decodeURIComponent(projectMatch[1])}/${decodeURIComponent(projectMatch[2])}`);
	if (id === null) throw new HttpError(400, "a project is <owner>/<name>");
	const key = projectKey(id);
	const rest = projectMatch[3] ?? "";
	if (caller.project !== undefined && caller.project !== key)
		throw new HttpError(403, `this agent token (\`${caller.agent}\`) is for ${caller.project} only`);
	const project = projectStub(env, id);
	const info = await project.info();
	if (info === null) throw new HttpError(404, `no project \`${key}\``);
	const member = await project.isMember(login);

	// Credentials are answered for non-members too: a public tracker is
	// readable by anyone logged in, and a refusal should say why.
	if (method === "POST" && rest === "/credentials") {
		const { repo, repos, scope = "read" } = await body<{
			repo?: string;
			repos?: string[];
			scope?: "read" | "write";
		}>(request);
		if (scope !== "read" && scope !== "write") throw new HttpError(400, "scope is read or write");
		const one = async (name: string) => {
			const ref = parseRepoName(repoFromUrlOrName(name));
			if (ref === null || projectKey(ref.project) !== key)
				throw new HttpError(400, `\`${name}\` is not one of ${key}'s repos`);
			if (caller.instance !== undefined && scope === "write" && ref.kind === "fork")
				requireThisInstance(caller, ref.instance);
			return credential(env, deps, project, ref, scope, caller, member);
		};
		// Several at once, minted together: each answered, or refused with the
		// status it would have had alone.
		if (Array.isArray(repos)) {
			if (repos.length === 0 || repos.length > 50 || repos.some((name) => typeof name !== "string"))
				throw new HttpError(400, "repos is a list of one to fifty repo names");
			const answers = await Promise.all(
				repos.map((name) =>
					one(name).catch((error: unknown) => {
						if (!(error instanceof HttpError)) throw error;
						return { repo: name, status: error.status, error: error.message };
					}),
				),
			);
			return json({ credentials: answers });
		}
		if (!repo) throw new HttpError(400, "repo is required (a repo name, or a remote URL)");
		return json(await one(repo));
	}

	// Deleting a project takes everything with it. Its record goes first, so
	// every request after this one finds no project; then the tokens pinned
	// to it; then its repos, and with each repo every password minted for it.
	if (method === "DELETE" && rest === "") {
		requireHuman(caller, "delete projects");
		// An admin may too, member or not, so the operator can take down what
		// should not be here.
		if (!listed(env.ADMIN_LOGINS, login)) requireOwner(info.owner, login, `delete ${key}`);
		const holders = new Set([
			...(await project.members()).map((m) => m.login),
			...(await project.instances()).map((i) => i.owner),
		]);
		await project.destroy();
		await userStub(env, info.owner).releaseProject(key);
		for (const holder of holders) await revokeRepoTokens(deps, await userStub(env, holder).revokeProject(key));
		// Parts never contain dots, so this prefix is this project's and no other's.
		const names = await deps.repos.names(`${id.owner}.${id.name}.`);
		for (const name of names) await deps.repos.delete(name);
		return json({ key, deleted: true, repos_deleted: names.length });
	}

	if (!member) throw new HttpError(403, `${login} is not a member of ${key}`);

	if (method === "GET" && rest === "") {
		return json({
			...info,
			key,
			endpoint: endpointUrl(url, id),
			members: await project.members(),
			branches: await project.branches(),
		});
	}

	// ── Members ────────────────────────────────────────────────────────────
	if (method === "POST" && rest === "/members") {
		requireHuman(caller, "add members");
		requireOwner(info.owner, login, "add members");
		const { login: added } = await body<{ login?: string }>(request);
		if (!added) throw new HttpError(400, "login is required");
		const normalized = added.toLowerCase();
		requirePart("login", normalized);
		return json({ login: normalized, added: await project.addMember(normalized, login) });
	}
	const memberMatch = rest.match(/^\/members\/([^/]+)$/);
	if (method === "DELETE" && memberMatch) {
		requireHuman(caller, "remove members");
		requireOwner(info.owner, login, "remove members");
		const removed = decodeURIComponent(memberMatch[1]).toLowerCase();
		if (removed === info.owner) throw new HttpError(409, "the project owner cannot be removed");
		return json({ login: removed, removed: await project.removeMember(removed) });
	}

	// ── Trackers in Git ──────────────────────────────────────────────────────
	// A tracker in Git carved after the project was created gets its repo here,
	// seeded once by whoever carved it. It joins a branch only by a merge.
	if (method === "POST" && rest === "/trackers") {
		// A new tracker in Git comes with its definition, which is a change to
		// the tracker that owns its file: only that tracker's maintainers add it.
		requireHuman(caller, "create tracker in Git repos");
		const { name: tracker } = await body<{ name?: string }>(request);
		if (!tracker) throw new HttpError(400, "name is required");
		requirePart("tracker name", tracker);
		await requireGovernor(deps, id, info, { login, member }, tracker, await listedTrackers(project));
		const defined = await definitionFacts(deps.repos, id, tracker, await listedTrackers(project));
		if (defined !== undefined && !defined.git)
			throw new HttpError(400, `\`${tracker}\` is not in Git; its repo is made by its first sync`);
		const ref: RepoRef = { kind: "tracker", project: id, tracker };
		if ((await deps.repos.remote(repoName(ref))) !== null) {
			// Made before by a merge or a first publication that did not
			// finish: until it is on a branch, its owner may seed it again.
			const listed = (await project.branches()).some((branch) => tracker in branch.list);
			if (listed)
				throw new HttpError(409, `${repoName(ref)} exists already; its history moves only through \`newgit merge\``);
			return json(await seed(env, deps, caller, ref, false), 200);
		}
		return json(await seed(env, deps, caller, ref), 201);
	}

	// ── Branches ───────────────────────────────────────────────────────────
	if (method === "GET" && rest === "/branches")
		return json({ default: info.defaultBranch, branches: await project.branches() });
	if (method === "PUT" && rest === "/branches/default") {
		requireOwner(info.owner, login, "change the default branch");
		requireNotInstance(caller, "change the default branch");
		const { name: branch } = await body<{ name?: string }>(request);
		if (!branch) throw new HttpError(400, "name is required");
		if (!(await project.setDefaultBranch(branch))) throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
		return json({ default: branch });
	}
	const mergeMatch = rest.match(/^\/branches\/([^/]+)\/merge$/);
	if (method === "POST" && mergeMatch) {
		// Merging is a person's decision: an agent proposes, a member merges.
		requireHuman(caller, "merge");
		const branch = decodeURIComponent(mergeMatch[1]);
		const { base, list } = await body<{
			base?: Record<string, string>;
			list?: Record<string, string>;
		}>(request);
		if (!base || typeof base !== "object" || !list || typeof list !== "object")
			throw new HttpError(400, "base (the index you merged onto) and list (the index the merge makes) are required");
		if (typeof list.root !== "string" || list.root === base.root)
			throw new HttpError(400, "a merge moves `root`: the branch is root's ref, so the new index names a new root commit");
		for (const [tracker, commit] of Object.entries(list)) {
			requirePart("tracker name", tracker);
			if (typeof commit !== "string" || !/^[0-9a-f]{40}$/.test(commit))
				throw new HttpError(400, `\`${tracker}\` must name a full commit id`);
		}
		const trackers = [...new Set([...Object.keys(base), ...Object.keys(list)])].filter(
			(tracker) => base[tracker] !== list[tracker],
		);
		// Every pin that moves is checked here, by Git, in the endpoint's
		// container: a move forward along that tracker's own `main` changes
		// only root, so root's maintainers may make it; anything else — back,
		// sideways, or to a commit `main` does not have — is that tracker's
		// maintainers' to make, and a commit off `main` gets them a write
		// password for that tracker: a merge into `main` moves its `main` to
		// catch up, one into another branch its branch of that name. Without the container
		// (a test, or one that cannot start), every move counts as not
		// forward: the safe answer.
		const children = trackers.filter((tracker) => tracker !== "root" && tracker in base && tracker in list);
		const moves = new Map<string, AncestryAnswer>();
		if (children.length > 0) {
			const checks = children.map((tracker) => ({ tracker, old: base[tracker], new: list[tracker] }));
			// Read from Artifacts here, in milliseconds; only a walk too long
			// to make here goes to the container's Git.
			let answers: AncestryAnswer[] | Checking | typeof ASK_GIT = await nativeAncestry(
				deps.repos,
				(tracker) => repoName({ kind: "tracker", project: id, tracker }),
				checks,
			);
			if (answers === ASK_GIT && deps.ancestry) {
				const plan: AncestryPlan = { dir: "/work", repos: checkRepos(deps, id, children), checks };
				// The first time a large history is there the check takes
				// minutes: the client asks again until it has the answer.
				answers = await deps.ancestry(key, plan, () => checkCredentials(env, deps, caller, id, children));
				if (isChecking(answers)) return stillChecking();
			}
			if (answers !== ASK_GIT) for (const answer of answers) moves.set(answer.tracker, answer);
		}
		// Everything is checked before the lock is taken: a refused merge
		// holds nothing.
		const who = { login, member };
		const writes: string[] = [];
		for (const tracker of trackers) {
			const facts = await definitionFacts(deps.repos, id, tracker, await listedTrackers(project));
			const dropped = !(tracker in list);
			if (!(tracker in base)) {
				// New to the branch: its definition lands with this very merge
				// (it is not on the default branch yet), and its history came in
				// through the repo seeded for it. Only whoever governs that
				// definition adds it.
				if (facts !== undefined && !facts.git)
					throw new HttpError(400, `\`${tracker}\` is not in Git; \`newgit merge\` never carries it`);
				if ((await deps.repos.remote(repoName({ kind: "tracker", project: id, tracker }))) === null)
					throw new HttpError(400, `\`${tracker}\` has no repo: a new tracker in Git joins by its own seeded repo`);
				if (!(await governs_(deps, id, info, who, tracker, await listedTrackers(project))))
					throw new HttpError(403, `only the maintainers of the tracker holding \`${tracker}\`'s definition may add it`);
				continue;
			}
			if (facts === undefined) {
				// A tracker the definitions no longer name (the migration
				// folding `project` into root) may only leave the index.
				if (dropped) continue;
				throw new HttpError(
					400,
					`\`${tracker}\` is not defined on \`access\`'s \`main\`: a new tracker in Git joins by its own seeded repo`,
				);
			}
			if (!facts.git) throw new HttpError(400, `\`${tracker}\` is not in Git; \`newgit merge\` never carries it`);
			const read = decide({ kind: "tracker", project: id, tracker }, "read", who, { tracker: facts });
			if (!read.ok) throw new HttpError(403, read.reason);
			const answer = moves.get(tracker);
			const forwardOnMain = !dropped && answer?.forward === true && answer.onMain === true;
			if (tracker !== "root" && forwardOnMain) continue;
			// Merging a tracker is its maintainers' (by default, its readers'):
			// `access`, which holds every definition, is the owner's unless the
			// definitions say otherwise.
			const may = mayMaintain(facts.maintainers, who, info.owner, tracker);
			if (!may.ok) {
				const why =
					tracker === "root" || dropped
						? may.reason
						: `${may.reason} (its pin moves ${answer?.forward ? "to a commit not on its `main`" : "back or sideways along its history"})`;
				throw new HttpError(403, why);
			}
			if (tracker === "root" || (!dropped && answer?.onMain !== true)) writes.push(tracker);
		}
		const started = await project.beginMerge(branch, base, login, MERGE_PENDING_SECONDS, list, writes);
		if (!started.ok) {
			if (started.reason === "missing") throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
			throw new HttpError(409, `\`${branch}\` moved since you looked: merge again onto what it is now`);
		}
		await revokeRepoTokens(deps, started.stale);
		const credentials = [];
		const handed = [];
		for (const tracker of writes) {
			const ref: RepoRef = { kind: "tracker", project: id, tracker };
			const name = repoName(ref);
			if ((await deps.repos.remote(name)) === null) throw new HttpError(404, `${name} does not exist`);
			// Enough to push to landing refs, and cancelled when the merge lands.
			const minted = await deps.repos.mint(name, "write", MERGE_PUSH_SECONDS);
			handed.push({ repo: name, artifactsId: minted.id });
			await userStub(env, login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
			credentials.push({
				tracker,
				repo: name,
				remote: deps.repos.url(name),
				username: "x",
				password: minted.password,
				expires_at: minted.expiresAt,
			});
		}
		await project.recordMergeTokens(branch, list, handed);
		return json({ branch, until: started.until, credentials });
	}

	const branchMatch = rest.match(/^\/branches\/([^/]+)$/);
	if (branchMatch) {
		const branch = decodeURIComponent(branchMatch[1]);
		if (method === "GET") {
			const found = await project.branch(branch);
			if (found === null) throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
			return json(found);
		}
		if (method === "PUT") {
			requireHuman(caller, "create or move branches");
			requirePart("branch name", branch);
			const { list, base, moves } = await body<{
				list?: Record<string, string>;
				base?: Record<string, string>;
				moves?: RefMove[];
			}>(request);
			if (!list || typeof list !== "object" || Object.keys(list).length === 0)
				throw new HttpError(400, "list is required: one commit per tracker in Git, by tracker name");
			for (const [tracker, commit] of Object.entries(list)) {
				requirePart("tracker name", tracker);
				if (typeof commit !== "string" || !/^[0-9a-f]{40}$/.test(commit))
					throw new HttpError(400, `\`${tracker}\` must name a full commit id`);
			}
			if (!("root" in list))
				throw new HttpError(400, "a branch is root's ref: its list must name `root`'s commit");
			if (base !== undefined) {
				// The end of a merge: the list moves from `base`, all at once.
				requireHuman(caller, "merge");
				// The index the merge began with, exactly: every pin in it was
				// checked then, so nothing else may land.
				const pending = await project.pendingMerge(branch, list);
				const current = (await project.branch(branch))?.list;
				// Landed long ago, its record gone: root's branch still follows.
				if (pending === null && !sameIndex(current, list))
					throw new HttpError(400, "this is not the index a merge began with: start the merge again");
				if (pending !== null && pending.by !== login)
					throw new HttpError(409, `${pending.by} began this merge; only they may land it`);
				const asked = checkMoves(moves, pending?.writes ?? [], branch, list);
				// And what root's commit really pins: the Git endpoint composes
				// from the gitlinks, so an index that says otherwise would have
				// had its pins checked for nothing.
				// A finish asked again after it landed (its answer never
				// arrived) checks nothing again, and follows again.
				if (pending?.landed !== true && !sameIndex(current, list)) {
					const trackers = Object.keys(list);
					// Each commit's pins, read from Artifacts here; the
					// container's Git only when a commit cannot be read.
					let checked: IndexAnswer | Checking | typeof ASK_GIT = await nativeIndex(
						deps.repos,
						(tracker) => repoName({ kind: "tracker", project: id, tracker }),
						list,
					);
					if (checked === ASK_GIT && deps.index) {
						const plan: IndexPlan = { dir: "/work", repos: checkRepos(deps, id, trackers), list };
						checked = await deps.index(key, plan, () => checkCredentials(env, deps, caller, id, trackers));
						// Still running: the client asks again. Nothing waits meanwhile.
						if (isChecking(checked)) return stillChecking();
					}
					if (checked !== ASK_GIT && checked.matches !== true)
						throw new HttpError(
							400,
							"root's commit does not pin what this index says: push the root commit the merge made to its landing ref, then finish",
						);
				}
				const landed = await landAndFollow(deps, project, id, branch, base, list, login, asked, true);
				if (!landed.ok) {
					if (landed.reason === "missing") throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
					throw new HttpError(409, `\`${branch}\` moved since you looked: merge again onto what it is now`);
				}
				return json({ ...landed.branch, behind: landed.behind });
			}
			const created = await project.createBranch(branch, list, login);
			if (created === null)
				throw new HttpError(409, `branch \`${branch}\` exists; a branch moves only through \`newgit merge\``);
			return json(created, 201);
		}
	}

	// ── Instances ──────────────────────────────────────────────────────────
	if (method === "POST" && rest === "/instances") {
		requireNotInstance(caller, "create instances");
		const { name, base_rev, private: isPrivate } = await body<{
			name?: string;
			base_rev?: string;
			private?: boolean;
		}>(request);
		if (!name || !base_rev) throw new HttpError(400, "name and base_rev are required");
		requirePart("instance name", name);
		const created = await project.createInstance(name, login, base_rev, caller.agent ?? null, isPrivate === true);
		if (created === null) throw new HttpError(409, `instance \`${name}\` already exists in ${key}`);
		// An instance forks a tracker the first time it changes it; only
		// root's is started early, below. The workspace's own credential is minted here so
		// nobody has to: it writes this instance and nothing else, renews
		// itself, and is revoked when the instance is removed.
		const instanceToken = await issue(env, {
			kind: "agent",
			login,
			// Names an agent only when one spawned it: a person running
			// `newgit checkpoint` in the workspace is not an agent.
			agent: caller.agent,
			project: key,
			instance: name,
			ttlSeconds: AGENT_TTL_SECONDS,
		});
		// Root is what nearly every instance changes first, and its fork is a
		// copy that takes seconds. Start it now, while the agent works, so the
		// first checkpoint finds it made rather than waiting on it.
		if (deps.copy && deps.prefork)
			deps.waitUntil(
				makeFork(env, deps, project, id, key, caller, name, "root").catch((error: unknown) =>
					console.error(JSON.stringify({ prefork: name, why: error instanceof Error ? error.message : String(error) })),
				),
			);
		return json(
			{ ...created, endpoint: endpointUrl(url, id), token: instanceToken.token, token_id: instanceToken.id },
			201,
		);
	}
	if (method === "GET" && rest === "/instances")
		return json({ instances: (await project.instances()).filter((instance) => visibleTo(instance, login)) });

	const forkMatch = rest.match(/^\/instances\/([^/]+)\/trackers\/([^/]+)\/fork$/);
	if (method === "POST" && forkMatch) {
		const name = decodeURIComponent(forkMatch[1]);
		const tracker = decodeURIComponent(forkMatch[2]);
		const instance = await project.instance(name);
		if (instance === null || !visibleTo(instance, login)) throw new HttpError(404, `no instance \`${name}\` in ${key}`);
		requireOwner(instance.owner, login, `fork for \`${name}\``);
		requireThisInstance(caller, name);
		const facts = await definitionFacts(deps.repos, id, tracker, await listedTrackers(project));
		if (facts === undefined) throw new HttpError(404, `tracker \`${tracker}\` is not defined on ${key}'s \`access\` \`main\``);
		if (!facts.git) throw new HttpError(400, `\`${tracker}\` is not in Git; instances do not fork it`);
		const forked = await makeFork(env, deps, project, id, key, caller, name, tracker);
		if (forked === "forking")
			return Response.json({ forking: true }, { status: 202, headers: { "retry-after": String(FORK_RETRY_SECONDS) } });
		return json({ instance: name, tracker, repo: forked, remote: deps.repos.url(forked) });
	}

	const instanceMatch = rest.match(/^\/instances\/([^/]+)(\/head)?$/);
	if (instanceMatch) {
		const name = decodeURIComponent(instanceMatch[1]);
		const instance = await project.instance(name);
		if (instance === null || !visibleTo(instance, login)) throw new HttpError(404, `no instance \`${name}\` in ${key}`);
		if (method === "GET" && !instanceMatch[2]) return json(instance);
		if (method === "PUT" && instanceMatch[2]) {
			requireOwner(instance.owner, login, `move \`${name}\`'s head`);
			requireThisInstance(caller, name);
			const { head_rev } = await body<{ head_rev?: string }>(request);
			if (!head_rev) throw new HttpError(400, "head_rev is required");
			return json(await project.setHead(name, head_rev));
		}
		if (method === "DELETE" && !instanceMatch[2]) {
			requireOwner(instance.owner, login, `remove \`${name}\``);
			requireNotInstance(caller, "remove instances");
			for (const tracker of await project.forkRepos(name))
				await deps.repos.delete(repoName({ kind: "fork", project: id, instance: name, tracker })).catch(() => false);
			await project.deleteInstance(name);
			await revokeRepoTokens(deps, await userStub(env, instance.owner).revokeInstance(key, name));
			return json({ name, removed: true });
		}
	}

	// ── Checkpoints ────────────────────────────────────────────────────────
	if (method === "GET" && rest === "/checkpoints") {
		const instance = url.searchParams.get("instance") ?? undefined;
		return json({ checkpoints: await project.checkpoints(instance, login) });
	}
	const checkpointMatch = rest.match(/^\/checkpoints\/([^/]+)\/([^/]+)$/);
	if (checkpointMatch) {
		const instance = decodeURIComponent(checkpointMatch[1]);
		const checkpoint = decodeURIComponent(checkpointMatch[2]);
		if (method === "GET") {
			const owner = await project.instance(instance);
			const found = owner === null || visibleTo(owner, login) ? await project.checkpoint(instance, checkpoint) : null;
			if (found === null) throw new HttpError(404, `no checkpoint ${instance}/${checkpoint} in ${key}`);
			return json(found);
		}
		if (method === "PUT") {
			const found = await project.instance(instance);
			const owner = found !== null && visibleTo(found, login) ? found.owner : undefined;
			if (owner === undefined) throw new HttpError(404, `no instance \`${instance}\` in ${key}`);
			requireOwner(owner, login, `checkpoint \`${instance}\``);
			requireThisInstance(caller, instance);
			const { record, head_rev } = await body<{ record?: unknown; head_rev?: string }>(request);
			if (record === undefined) throw new HttpError(400, "record is required");
			const saved = await project.putCheckpoint(instance, checkpoint, login, caller.agent ?? null, record);
			// The instance's head moves with its checkpoint: one request, not two.
			if (typeof head_rev === "string" && head_rev !== "") await project.setHead(instance, head_rev);
			return json(saved, 201);
		}
	}

	throw new HttpError(404, `no route for ${method} ${path}`);
}

// ── The Git endpoint ──────────────────────────────────────────────────────

/**
 * Where a Git request's time went, as a `Server-Timing` header any client can
 * read with `GIT_TRACE_CURL=1`: durations and counts only, never a name.
 * `endpoint` ends when the container's response starts, so a body it streams
 * is not in it. Time in one stage adds up across its calls.
 */
class Timing {
	private readonly started = Date.now();
	private readonly spent = new Map<string, number>();
	private readonly counts = new Map<string, number>();

	async span<T>(stage: string, work: () => Promise<T>): Promise<T> {
		const start = Date.now();
		try {
			return await work();
		} finally {
			this.spent.set(stage, (this.spent.get(stage) ?? 0) + Date.now() - start);
		}
	}

	set(name: string, value: number) {
		this.counts.set(name, value);
	}

	/** `repos`, counting what a request asks Artifacts for. */
	counting(repos: Repos): Repos {
		const count = (name: string) => this.counts.set(name, (this.counts.get(name) ?? 0) + 1);
		return new Proxy(repos, {
			get: (target, property, receiver) => {
				const value = Reflect.get(target, property, receiver);
				if (typeof value !== "function") return value;
				return (...args: unknown[]) => {
					if (property === "readFile") count("reads");
					if (property === "mint") count("mints");
					return value.apply(target, args);
				};
			},
		});
	}

	/** `response`, saying so. Git ignores the header. */
	on(response: Response): Response {
		const stages = [...this.spent].map(([stage, ms]) => `${stage};dur=${ms}`);
		const counts = [...this.counts].map(([name, value]) => `${name};desc="${value}"`);
		const header = [`total;dur=${Date.now() - this.started}`, ...stages, ...counts].join(", ");
		const timed = new Response(response.body, response);
		timed.headers.set("server-timing", header);
		return timed;
	}
}

/**
 * Read passwords a Git request already minted, in this isolate's memory only.
 * Minting and recording one is most of a request (0.5s of 0.75s with three
 * trackers), for a password that lasts an hour.
 *
 * Reusing one decides nothing about access: every request still authenticates
 * its token and asks who may read each tracker, and only then looks here, so a
 * revoked token or a removed member never reaches it. These go to the endpoint
 * container, never to the caller. A key holds the token, as a password's
 * record does, so revoking that token revokes what was reused.
 */
class ReadPasswords {
	private readonly passwords = new Map<string, { password: string; until: number }>();

	/** One this close to expiring could lapse during a long fetch. */
	private static readonly MARGIN_MS = 10 * 60_000;
	private static readonly MOST = 2000;

	get(key: string): string | undefined {
		const held = this.passwords.get(key);
		if (held === undefined) return undefined;
		if (held.until - ReadPasswords.MARGIN_MS > Date.now()) return held.password;
		this.passwords.delete(key);
		return undefined;
	}

	set(key: string, password: string, expiresAt: string) {
		if (this.passwords.size >= ReadPasswords.MOST) {
			const oldest = this.passwords.keys().next();
			if (!oldest.done) this.passwords.delete(oldest.value);
		}
		this.passwords.set(key, { password, until: Date.parse(expiresAt) });
	}
}

const readPasswords = new ReadPasswords();

/**
 * Credentials the credentials route minted, in this isolate's memory, for the
 * same reason as [`ReadPasswords`]: a checkpoint asks for one per tracker it
 * syncs, and minting and recording each was most of a second. Every request
 * still decides what the caller may do first, and only then looks here; a key
 * holds the token, so revoking it revokes what was reused.
 */
class MintedPasswords {
	private readonly held = new Map<string, { password: string; expiresAt: string; remote: string }>();

	/** One this close to expiring could lapse during a long push. */
	private static readonly MARGIN_MS = 10 * 60_000;
	private static readonly MOST = 2000;

	get(key: string): { password: string; expiresAt: string; remote: string } | undefined {
		const held = this.held.get(key);
		if (held === undefined) return undefined;
		if (Date.parse(held.expiresAt) - MintedPasswords.MARGIN_MS > Date.now()) return held;
		this.held.delete(key);
		return undefined;
	}

	set(key: string, password: string, expiresAt: string, remote: string) {
		if (this.held.size >= MintedPasswords.MOST) {
			const oldest = this.held.keys().next();
			if (!oldest.done) this.held.delete(oldest.value);
		}
		this.held.set(key, { password, expiresAt, remote });
	}
}

const mintedPasswords = new MintedPasswords();

/** `/<owner>/<name>.git/info/refs` and `/<owner>/<name>.git/git-upload-pack`. */
const GIT_ROUTE = /^\/([^/]+)\/([^/]+?)\.git\/(info\/refs|git-upload-pack|git-receive-pack)$/;

/**
 * Answer a Git client: `git clone https://newgit.dev/<owner>/<name>.git`
 * with plain Git. The password is a newgit token (the credential helper gives
 * a workspace's own); each branch is composed from the trackers this caller
 * may read, each fetched with a short read password minted for them and
 * recorded against their token, so revoking the token revokes those too.
 * Read-only: pushes go through a newgit workspace, which splits them.
 */
async function gitEndpoint(request: Request, env: Env, deps: Deps, match: RegExpMatchArray): Promise<Response> {
	const timing = new Timing();
	const response = await answerGit(request, env, { ...deps, repos: timing.counting(deps.repos) }, match, timing);
	return timing.on(response);
}

async function answerGit(
	request: Request,
	env: Env,
	deps: Deps,
	match: RegExpMatchArray,
	timing: Timing,
): Promise<Response> {
	const url = new URL(request.url);
	const service = match[3] === "info/refs" ? url.searchParams.get("service") : match[3];
	if (service === "git-receive-pack")
		throw new HttpError(
			403,
			"this endpoint is read-only: push from a newgit workspace, which splits your work into its trackers",
		);
	if (service !== "git-upload-pack") throw new HttpError(400, "only Git's smart HTTP protocol is served here");

	const header = request.headers.get("authorization") ?? "";
	let token = "";
	if (header.startsWith("Basic ")) {
		let decoded = "";
		try {
			decoded = atob(header.slice("Basic ".length));
		} catch {
			throw new HttpError(401, "that Authorization header is not Basic auth");
		}
		token = decoded.slice(decoded.indexOf(":") + 1);
	} else if (header.startsWith("Bearer ")) {
		token = header.slice("Bearer ".length);
	}
	if (!token) throw new HttpError(401, "log in with `newgit login`; the credential helper answers for this URL");
	const caller = await timing.span("auth", () => callerFor(token, env));

	let id: ProjectId | null = null;
	try {
		id = parseProjectKey(`${decodeURIComponent(match[1])}/${decodeURIComponent(match[2])}`);
	} catch {
		id = null;
	}
	if (id === null) throw new HttpError(404, "a project is <owner>/<name>.git");
	const key = projectKey(id);
	if (caller.project !== undefined && caller.project !== key)
		throw new HttpError(403, `this token is for ${caller.project} only`);
	const project = projectStub(env, id);
	const info = await timing.span("project", () => project.info());
	if (info === null) throw new HttpError(404, `no project \`${key}\``);
	const member = await timing.span("project", () => project.isMember(caller.login));
	const who = { login: caller.login, member };

	const branches = await timing.span("project", () => project.branches());
	const main = branches.find((branch) => branch.name === info.defaultBranch)?.list;
	if (!main?.root) throw new HttpError(404, `${key} has no \`${info.defaultBranch}\` branch yet`);

	// Policy, read as the remote reads it: each definition from the tracker
	// that owns its file, at that tracker's `main`. Shape (pins, mounts) is the
	// container's to read from the commits themselves.
	const facts = await timing.span("definitions", () =>
		loadDefinitions(
			deps.repos,
			id,
			branches.flatMap((branch) => Object.keys(branch.list)),
		),
	);
	const readable = (tracker: string) => {
		const tracker_facts = facts.get(tracker);
		return (
			tracker_facts?.git === true &&
			decide({ kind: "tracker", project: id, tracker }, "read", who, { tracker: tracker_facts }).ok
		);
	};

	// Each branch, cut down to the trackers in Git this caller may read. A
	// branch is root's ref, so one whose root they cannot read is not served.
	const plan: EndpointPlan = { dir: "/work", branches: {}, default_branch: info.defaultBranch, repos: {} };
	// `access` always: the container reads every mount from its content,
	// even for a reader who may not read it. It is served only where listed.
	const needed = new Set<string>(["access"]);
	for (const branch of branches) {
		if (!branch.list.root || !readable("root")) continue;
		const list: Record<string, string> = {};
		for (const [tracker, commit] of Object.entries(branch.list)) {
			if (!readable(tracker)) continue;
			list[tracker] = commit;
			needed.add(tracker);
		}
		plan.branches[branch.name] = list;
	}
	if (Object.keys(plan.branches).length === 0)
		throw new HttpError(who.member ? 403 : 404, who.member ? `nothing in ${key} is yours to read` : `no project \`${key}\``);

	for (const tracker of needed) {
		const name = repoName({ kind: "tracker", project: id, tracker });
		const key = `${caller.tokenId}:${name}`;
		let password = readPasswords.get(key);
		if (password === undefined) {
			const minted = await timing.span("mint", () => deps.repos.mint(name, "read", READ_CREDENTIAL_TTL_SECONDS));
			await timing.span("record", () =>
				userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt),
			);
			password = minted.password;
			readPasswords.set(key, password, minted.expiresAt);
		}
		plan.repos[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${password}`)}` };
	}
	timing.set("branches", Object.keys(plan.branches).length);
	timing.set("trackers", needed.size);
	return timing.span("endpoint", () =>
		deps.endpoint(key, service === match[3] ? "upload-pack" : "advertise", plan, request),
	);
}

/** Where `/` sends a visitor when this remote publishes no landing page. */
const REPOSITORY = "https://github.com/Spheroman/newgit";

/**
 * The landing page: one file of one tracker, at that tracker's `main`, named
 * by `LANDING_PAGE` in wrangler.jsonc. newgit.dev's is in `site`, a tracker
 * outside Git, so this route is the one place its content is published, and
 * only the file the config names. Publishing a change is `newgit tracker
 * merge site`, which moves the repo's branch `main` (its default branch, so
 * what `HEAD` reads); no deploy. Only a merge creates that branch, so a
 * tracker never merged serves nothing.
 */
async function landingPage(method: string, env: Env, repos: Repos): Promise<Response> {
	const page = env.LANDING_PAGE;
	const project = page ? parseProjectKey(page.project) : null;
	const html =
		page && project ? await repos.readFile(repoName({ kind: "tracker", project, tracker: page.tracker }), "HEAD", page.path) : null;
	if (html === null) return Response.redirect(REPOSITORY, 302);
	return new Response(method === "HEAD" ? null : html, {
		headers: { "content-type": "text/html; charset=utf-8", "cache-control": "public, max-age=60" },
	});
}

/** Where a project's Git endpoint is: `https://<host>/<owner>/<name>.git`. */
function endpointUrl(url: URL, id: ProjectId): string {
	return `${url.origin}/${id.owner}/${id.name}.git`;
}

/** Create the repo of a tracker in Git and mint the one short write that seeds it. */
async function seed(env: Env, deps: Deps, caller: Caller, ref: RepoRef, create = true) {
	const name = repoName(ref);
	const remote = create ? (await deps.repos.create(name)).remote : deps.repos.url(name);
	const minted = await deps.repos.mint(name, "write", CREDENTIAL_TTL_SECONDS);
	await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
	return {
		tracker: ref.kind === "tracker" ? ref.tracker : "",
		repo: name,
		remote,
		username: "x",
		password: minted.password,
		expires_at: minted.expiresAt,
	};
}

/** Mint a Git credential for one repo, if the rules allow it. */
async function credential(
	env: Env,
	deps: Deps,
	project: DurableObjectStub<ProjectDO>,
	ref: RepoRef,
	scope: "read" | "write",
	caller: Caller,
	member: boolean,
) {
	const who = { login: caller.login, member };
	const facts: Facts = {};
	// Independent lookups, at once.
	const [instance, listed, complete] = await Promise.all([
		ref.kind === "fork" ? project.instance(ref.instance) : null,
		listedTrackers(project),
		ref.kind === "fork" ? project.forkComplete(ref.instance, ref.tracker) : true,
	]);
	if (ref.kind === "fork") {
		facts.instanceOwner = instance?.owner;
		facts.instancePrivate = instance?.private;
	}
	facts.tracker = await definitionFacts(deps.repos, ref.project, ref.tracker, listed);
	// Not a refusal but an absence, as on the fork route: a client may wait
	// for the definition to land (a checkpoint leaves the tracker for later),
	// while a 403 is a "no" it must not wait out.
	if (facts.tracker === undefined)
		throw new HttpError(404, `tracker \`${ref.tracker}\` is not defined on ${projectKey(ref.project)}'s \`access\` \`main\``);

	const decision = decide(ref, scope, who, facts);
	if (!decision.ok) throw new HttpError(403, decision.reason);

	const name = repoName(ref);
	// A fork whose copy is not done yet holds only part of its tracker: it
	// is not there to write, or read, until the fork route says it is.
	if (!complete) throw new HttpError(404, `${name} does not exist yet`);
	// Minted for this token already and not near expiring: hand it out
	// again, after every check above, as the Git endpoint does.
	const key = `${caller.tokenId}:${scope}:${name}`;
	const held = mintedPasswords.get(key);
	if (held !== undefined)
		return { repo: name, remote: held.remote, scope, username: "x", password: held.password, expires_at: held.expiresAt };
	// The repos of trackers in Git are seeded with the project, and forks
	// made by the fork route, so only a tracker outside Git is looked up: its
	// repo is created on its first write. Any other missing repo is the
	// mint's 404.
	let remote = deps.repos.url(name);
	if (ref.kind === "tracker" && !facts.tracker?.git) {
		const found = await deps.repos.remote(name);
		if (found !== null) remote = found;
		else if (scope === "write") remote = (await deps.repos.create(name)).remote;
		else throw new HttpError(404, `${name} does not exist yet`);
	}
	const minted = await deps.repos
		.mint(name, scope, scope === "read" ? READ_CREDENTIAL_TTL_SECONDS : CREDENTIAL_TTL_SECONDS)
		.catch((error: unknown) => {
			if ((error as { code?: string }).code === "NOT_FOUND") throw new HttpError(404, `${name} does not exist yet`);
			throw error;
		});
	await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
	mintedPasswords.set(key, minted.password, minted.expiresAt, remote);
	return { repo: name, remote, scope, username: "x", password: minted.password, expires_at: minted.expiresAt };
}

/** A tracker's committed definition, as the remote reads it. */
export interface TrackerFacts {
	audience: Audience;
	git: boolean;
	maintainers: Maintainers;
	/** Where its root sits in the view: `.` unless the definition says. */
	mount: string;
	/** Its claims as written, relative to `mount`; `!` excludes. */
	paths: string[];
	/** The tracker whose history holds this definition's file. */
	source: string;
}

/** Where `access` is mounted: every definition is `<name>.toml` at its root. */
const ACCESS_MOUNT = ".newgit/trackers";

/** Trackers that always exist, file or not. */
function implicitFacts(tracker: string): TrackerFacts | undefined {
	switch (tracker) {
		case "root":
			return {
				audience: { kind: "project-devs" },
				git: true,
				maintainers: { kind: "project-devs" },
				mount: ".",
				paths: ["."],
				source: "access",
			};
		case "access":
			return {
				audience: { kind: "project-devs" },
				git: true,
				maintainers: { kind: "owner" },
				mount: ACCESS_MOUNT,
				paths: ["."],
				source: "access",
			};
		default:
			return undefined;
	}
}

function parseFacts(text: string, source: string): TrackerFacts | undefined {
	let definition: { audience?: unknown; git?: unknown; maintainers?: unknown; mount?: unknown; paths?: unknown };
	try {
		definition = parseToml(text) as typeof definition;
	} catch {
		return undefined;
	}
	const audience = typeof definition.audience === "string" ? parseAudience(definition.audience) : null;
	if (audience === null) return undefined;
	// `maintainers` defaults to the audience: anyone who may read it may change it.
	const maintainers =
		typeof definition.maintainers === "string"
			? parseMaintainers(definition.maintainers)
			: parseMaintainers(typeof definition.audience === "string" ? definition.audience : "");
	if (maintainers === null) return undefined;
	const mount = normalizePath(typeof definition.mount === "string" ? definition.mount : ".");
	const paths = Array.isArray(definition.paths) ? definition.paths.filter((path) => typeof path === "string") : [];
	// `git` defaults to false, and is written only when true.
	return { audience, git: definition.git === true, maintainers, mount, paths, source };
}

/** `./a/b/` → `a/b`; the empty path is `.`. */
function normalizePath(path: string): string {
	const parts = path.split("/").filter((part) => part !== "" && part !== ".");
	return parts.length === 0 ? "." : parts.join("/");
}

/** Whether `path` is `prefix` or under it, by whole segments; `.` holds everything. */
function holds(prefix: string, path: string): boolean {
	return prefix === "." || path === prefix || path.startsWith(`${prefix}/`);
}

/** A workspace path relative to the mount holding it (see [`holds`]). */
function relativeTo(mount: string, path: string): string {
	return mount === "." ? path : path === mount ? "." : path.slice(mount.length + 1);
}

/** A claim as written, laid onto its tracker's mount. */
function placeClaim(mount: string, claim: string): string {
	const relative = normalizePath(claim);
	return mount === "." ? relative : relative === "." ? mount : `${mount}/${relative}`;
}

/**
 * How specifically `facts` claims workspace `path`: its longest claim
 * covering it (`.` counting 0), or undefined when none does or one of its
 * exclusions takes it out.
 */
function claimOn(facts: TrackerFacts, path: string): number | undefined {
	let best: number | undefined;
	for (const written of facts.paths) {
		const excluded = written.startsWith("!");
		const claim = placeClaim(facts.mount, excluded ? written.slice(1) : written);
		if (!holds(claim, path)) continue;
		if (excluded) return undefined;
		const length = claim === "." ? 0 : claim.length;
		if (best === undefined || length > best) best = length;
	}
	return best;
}

/**
 * The tracker in Git owning workspace `path`, by the ownership rules
 * (docs/design/trackers.md, *Ownership*), as the client's `owner_of` decides it:
 * the deepest mount holding the path decides, a parent's claim not reaching
 * into it; among the trackers mounted there the longest claim wins, ties to
 * the lesser name; a path every claim there excludes is owned by nothing.
 */
function ownerOf(facts: Map<string, TrackerFacts>, path: string): string | undefined {
	const depth = (mount: string) => (mount === "." ? 0 : mount.split("/").length);
	const inGit = [...facts].filter(([, candidate]) => candidate.git && holds(candidate.mount, path));
	if (inGit.length === 0) return undefined;
	const deepest = Math.max(...inGit.map(([, candidate]) => depth(candidate.mount)));
	let owner: { name: string; claim: number } | undefined;
	for (const [name, candidate] of inGit) {
		if (depth(candidate.mount) !== deepest) continue;
		const claim = claimOn(candidate, path);
		if (claim === undefined) continue;
		if (owner === undefined || claim > owner.claim || (claim === owner.claim && name < owner.name))
			owner = { name, claim };
	}
	return owner?.name;
}

/**
 * Which tracker holds each definition file next, given who holds them now
 * (`holder`, tracker by tracker) and the definitions as read from those
 * holders (`facts`). Governance only delegates downward:
 *
 * - `access.toml` is always `access`'s, and no tracker holds its own.
 * - A tracker may take `<T>.toml` only from the tracker that holds its own
 *   definition. Its claim is then written in that holder's repo, so the
 *   maintainers who already govern `T` are the ones who approved it. A claim
 *   on anything else — `access`'s, a parent's, a sibling's held by someone
 *   else — does not count (#228).
 * - A tracker keeps what it holds while it still claims it: whoever governs
 *   it governs what it holds.
 * - No move closes a loop, so every chain of holders ends at `access`.
 *
 * Among the trackers that may take a file, the ownership rules decide
 * ([`ownerOf`]); when none claims it, it is `access`'s. Files are settled
 * one at a time in name order, each against the moves already made, so the
 * outcome is the same on every read.
 */
function nextHolders(facts: Map<string, TrackerFacts>, holder: Map<string, string>): Map<string, string> {
	const next = new Map(holder);
	const above = (name: string, target: string): boolean => {
		// Whether `target` is `name` or on its chain of holders.
		for (let at: string | undefined = name, steps = 0; at !== undefined && steps <= next.size; steps++) {
			if (at === target) return true;
			if (at === "access") return false;
			at = next.get(at);
		}
		return true; // a chain that does not end at `access` is never safe to join
	};
	for (const name of [...holder.keys()].sort()) {
		if (name === "access") {
			next.set(name, "access");
			continue;
		}
		const current = holder.get(name)!;
		const eligible = new Map<string, TrackerFacts>();
		for (const [candidate, candidateFacts] of facts) {
			if (candidate === name) continue;
			const delegated = (next.get(candidate) ?? "access") === current;
			const keeps = candidate === current;
			if (!delegated && !keeps) continue;
			if (above(candidate, name)) continue;
			eligible.set(candidate, candidateFacts);
		}
		next.set(name, ownerOf(eligible, `${ACCESS_MOUNT}/${name}.toml`) ?? "access");
	}
	return next;
}

/**
 * Every named tracker's committed definition on the project's default
 * branch. A definition is the view's `.newgit/trackers/<name>.toml`: `access`'s
 * `<name>.toml`, unless a tracker in Git owns that path by the ownership rules
 * and may hold it ([`nextHolders`]) — which is how governance layers:
 * changing a tracker's definition is changing the tracker that owns its file,
 * and only that tracker's maintainers merge it. The file is read from its
 * owner's repo, relative to the owner's mount. Resolved to a fixed point, a
 * few reads per round.
 *
 * A tracker `access` does not name may be defined inside a mounted tracker in
 * Git, in that tracker's own `.newgit/trackers/`: its child, governed by it
 * (see [`addNested`]). Its id, `t3code__repos`, names that parent, so a tracker
 * cannot define another's child, and `access` keeps every name it defines.
 */
/**
 * Definitions read before, by project and names, with the `main` each holder
 * was at when they were read. Reused only while every one of those is
 * still where it was, so a definition changed on `main` is read again on the
 * very next request: exact, not a time to live.
 */
const definitionsRead = new Map<string, { heads: Map<string, string>; facts: Map<string, TrackerFacts> }>();
const DEFINITIONS_KEPT = 200;

export async function loadDefinitions(
	repos: Repos,
	project: ProjectId,
	names: Iterable<string>,
): Promise<Map<string, TrackerFacts>> {
	const all = new Set<string>(["root", "access", ...names]);
	const key = `${projectKey(project)} ${[...all].sort().join(" ")}`;
	const known = definitionsRead.get(key);
	if (known !== undefined) {
		const now = await Promise.all([...known.heads].map(async ([repo, head]) => (await repos.branchHead(repo, "main")) === head));
		if (now.every(Boolean)) return new Map(known.facts);
		definitionsRead.delete(key);
	}
	// Each holder's `main`, read before any of its files: a file read after
	// `main` moves on is newer than the head it is kept under, which only
	// means reading it again next time.
	const heads = new Map<string, Promise<string | null>>();
	const reader: Repos = Object.create(repos);
	reader.readFile = async (name, ref, path) => {
		if (!heads.has(name)) heads.set(name, repos.branchHead(name, ref));
		await heads.get(name);
		return repos.readFile(name, ref, path);
	};
	const facts = await readDefinitions(reader, project, all);
	const settled = new Map<string, string>();
	let keep = true;
	for (const [name, head] of heads) {
		const at = await head;
		if (at === null) keep = false;
		else settled.set(name, at);
	}
	if (keep) {
		if (definitionsRead.size >= DEFINITIONS_KEPT) definitionsRead.delete(definitionsRead.keys().next().value!);
		definitionsRead.set(key, { heads: settled, facts: new Map(facts) });
	}
	return facts;
}

async function readDefinitions(
	repos: Repos,
	project: ProjectId,
	all: Set<string>,
): Promise<Map<string, TrackerFacts>> {
	// Policy is each definition holder's own `main` (`access`'s, or the
	// governing tracker's), whatever the project's default branch is called.
	const branch = "main";
	const file = (name: string) => `${ACCESS_MOUNT}/${name}.toml`;
	const source = new Map<string, { tracker: string; mount: string }>(
		[...all].map((name) => [name, { tracker: "access", mount: ACCESS_MOUNT }]),
	);
	let facts = new Map<string, TrackerFacts>();
	// Names a file already answers for, even one that does not parse: that
	// is an error to fix, not a name free for a nested definition to take.
	const written = new Set<string>();
	for (let round = 0; round < 4; round++) {
		facts = new Map();
		// Every file of a round at once: they are independent reads.
		const texts = await Promise.all(
			[...all].map(async (name) => {
				const from = source.get(name)!;
				const path = relativeTo(from.mount, file(name));
				let text =
					path === "." ? null : await repos.readFile(repoName({ kind: "tracker", project, tracker: from.tracker }), branch, path);
				// The layout before mounts kept `access`'s files at their
				// workspace paths. Read it too, so a remote deployed before a
				// project migrates (`newgit tracker migrate`) still reads its
				// definitions. Delete once every project on the remote migrated.
				if (text === null && from.tracker === "access")
					text = await repos.readFile(repoName({ kind: "tracker", project, tracker: "access" }), branch, file(name));
				return [name, from, text] as const;
			}),
		);
		for (const [name, from, text] of texts) {
			if (text !== null) written.add(name);
			const read = text === null ? implicitFacts(name) : parseFacts(text, from.tracker);
			if (read !== undefined) facts.set(name, { ...read, source: from.tracker });
		}
		const holders = nextHolders(facts, new Map([...source].map(([name, from]) => [name, from.tracker])));
		let moved = false;
		for (const name of all) {
			const owner = holders.get(name) ?? "access";
			const mount = facts.get(owner)?.mount ?? ACCESS_MOUNT;
			const was = source.get(name)!;
			if (was.tracker !== owner || was.mount !== mount) {
				source.set(name, { tracker: owner, mount });
				moved = true;
			}
		}
		if (!moved) break;
	}
	await addNested(repos, project, branch, facts, [...all].filter((name) => !written.has(name)));
	return facts;
}

/** What joins a tracker's id to its parent's, as `t3code__repos`. */
const ID_SEPARATOR = "__";

/**
 * Add the trackers in `names` that `facts` lacks and whose id says a mounted
 * tracker in Git defines them: `t3code__repos` is `repos.toml` in `t3code`'s
 * own tree, mounted relative to it, and held by it. The id names the one
 * place to look, so no other tracker can answer for it. A child can be a
 * parent, so this goes one level of mounts at a time.
 */
async function addNested(
	repos: Repos,
	project: ProjectId,
	branch: string,
	facts: Map<string, TrackerFacts>,
	names: string[],
): Promise<void> {
	const nested = names.filter((name) => name.includes(ID_SEPARATOR));
	nested.sort((left, right) => left.split(ID_SEPARATOR).length - right.split(ID_SEPARATOR).length);
	for (const name of nested) {
		const at = name.lastIndexOf(ID_SEPARATOR);
		const parent = name.slice(0, at);
		const holder = facts.get(parent);
		if (holder === undefined || !holder.git || holder.mount === "." || holder.mount === ACCESS_MOUNT) continue;
		const repo = repoName({ kind: "tracker", project, tracker: parent });
		const stem = name.slice(at + ID_SEPARATOR.length);
		const text = await repos.readFile(repo, branch, `${ACCESS_MOUNT}/${stem}.toml`);
		const read = text === null ? undefined : parseFacts(text, parent);
		if (read !== undefined) facts.set(name, { ...read, mount: placeClaim(holder.mount, read.mount) });
	}
}

/**
 * Whether `who` maintains the tracker that owns `tracker`'s definition file —
 * the one whose change adds or redefines `tracker`.
 */
async function governs_(
	deps: Deps,
	id: ProjectId,
	info: { owner: string; defaultBranch: string },
	who: { login: string; member: boolean },
	tracker: string,
	listed: Iterable<string>,
): Promise<boolean> {
	const definitions = await loadDefinitions(deps.repos, id, [tracker, ...listed]);
	const governor = definitions.get(tracker)?.source ?? "access";
	const facts = definitions.get(governor) ?? implicitFacts("access")!;
	return mayMaintain(facts.maintainers, who, info.owner, governor).ok;
}

async function requireGovernor(
	deps: Deps,
	id: ProjectId,
	info: { owner: string; defaultBranch: string },
	who: { login: string; member: boolean },
	tracker: string,
	listed: Iterable<string>,
) {
	if (!(await governs_(deps, id, info, who, tracker, listed)))
		throw new HttpError(
			403,
			`only the maintainers of the tracker holding \`${tracker}\`'s definition may add or redefine it`,
		);
}

/** Every tracker any branch of the project lists: whose definitions may layer. */
async function listedTrackers(project: DurableObjectStub<ProjectDO>): Promise<string[]> {
	return [...new Set((await project.branches()).flatMap((branch) => Object.keys(branch.list)))];
}

/** One tracker's definition (see [`loadDefinitions`]). */
async function definitionFacts(
	repos: Repos,
	project: ProjectId,
	tracker: string,
	listed: Iterable<string> = [],
): Promise<TrackerFacts | undefined> {
	return (await loadDefinitions(repos, project, [tracker, ...listed])).get(tracker);
}

/** Accept a bare repo name or anything ending in `/<repo>.git` (what a credential helper sees). */
function repoFromUrlOrName(value: string): string {
	if (!value.includes("/")) return value;
	const last = value.replace(/\/+$/, "").split("/").pop() ?? "";
	return last.endsWith(".git") ? last.slice(0, -".git".length) : last;
}

async function authenticate(request: Request, env: Env): Promise<Caller> {
	const header = request.headers.get("authorization") ?? "";
	const token = header.startsWith("Bearer ") ? header.slice("Bearer ".length) : "";
	return callerFor(token, env);
}

async function callerFor(token: string, env: Env): Promise<Caller> {
	const claims = token ? await verifyToken(env.TOKEN_KEY, token) : null;
	if (claims === null) throw new HttpError(401, "not logged in: run `newgit login`");
	// On every request, not only at login: taking someone off the list shuts
	// out the tokens they already hold, agents' included.
	await requireInvited(env, claims.sub);
	if (!(await userStub(env, claims.sub).check(claims.jti)))
		throw new HttpError(401, "this token was revoked: run `newgit login`, or ask its owner for a new agent token");
	return {
		login: claims.sub,
		tokenId: claims.jti,
		kind: claims.kind,
		...(claims.agent === undefined ? {} : { agent: claims.agent }),
		...(claims.project === undefined ? {} : { project: claims.project }),
		...(claims.instance === undefined ? {} : { instance: claims.instance }),
	};
}

async function requireInvited(env: Env, login: string) {
	if (!env.ALLOWED_LOGINS)
		throw new HttpError(503, "this remote admits no one yet: its operator has not set ALLOWED_LOGINS (`*` for anyone)");
	if (!listed(env.ALLOWED_LOGINS, login))
		throw new HttpError(403, `this remote is invite-only, and GitHub user \`${login}\` is not invited`);
	if (listed(env.BLOCKED_LOGINS, login))
		throw new HttpError(403, `GitHub user \`${login}\` is blocked from this remote`);
	if (listed(env.ADMIN_LOGINS, login)) return;
	const paused = await pausedBy(env);
	if (paused)
		throw new HttpError(
			503,
			`this remote is paused: it was billed $${paused.spent.toFixed(2)} in the last 7 days, over its $${paused.limit} weekly budget (as of ${paused.checkedAt}). It reopens on its own once that falls under the budget.`,
		);
}

/** How many projects one login may own: `PROJECTS_PER_LOGIN`, 5 when unset. */
function projectCap(env: Env): number {
	const cap = Number.parseInt(env.PROJECTS_PER_LOGIN ?? "", 10);
	return Number.isFinite(cap) && cap >= 0 ? cap : 5;
}

/** Sign a token and record it with its user, so it can later be revoked. */
export async function issue(env: Env, spec: NewToken) {
	const { token, claims } = await signToken(env.TOKEN_KEY, spec);
	const record = await userStub(env, claims.sub).register(claims);
	return { token, id: claims.jti, record };
}

function userStub(env: Env, login: string): DurableObjectStub<UserDO> {
	return env.USERS.getByName(login.toLowerCase());
}

/** A merge check's repos, without credentials: enough to ask after one already started. */
function checkRepos(deps: Deps, project: ProjectId, trackers: string[]): Record<string, { url: string; auth: string }> {
	return Object.fromEntries(
		trackers.map((tracker) => [
			tracker,
			{ url: deps.repos.url(repoName({ kind: "tracker", project, tracker })), auth: "" },
		]),
	);
}

/**
 * Read passwords for a merge check that is starting, recorded against the
 * caller's token so revoking it revokes them. They last as long as the
 * check may run, not the default fifteen minutes.
 */
async function checkCredentials(
	env: Env,
	deps: Deps,
	caller: Caller,
	project: ProjectId,
	trackers: string[],
): Promise<Record<string, { url: string; auth: string }>> {
	const repos: Record<string, { url: string; auth: string }> = {};
	for (const tracker of trackers) {
		const name = repoName({ kind: "tracker", project, tracker });
		const minted = await deps.repos.mint(name, "read", CHECK_CREDENTIAL_SECONDS);
		await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
		repos[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${minted.password}`)}` };
	}
	return repos;
}

/**
 * Make `fork` an empty repo for a copy to fill. One already there (a copy
 * that died part way) is kept: the copy carries on into it. One left stuck
 * by Artifacts' own fork, which this route used before, says
 * `FORK_IN_PROGRESS` forever (hours, on t3code): it is deleted and made
 * again. A refused delete is answered, not retried.
 */
async function emptyFork(deps: Deps, fork: string, artifacts: (step: string) => (error: unknown) => never) {
	let exists: boolean;
	try {
		exists = (await deps.repos.remote(fork)) !== null;
	} catch (error) {
		if ((error as { code?: string }).code !== "FORK_IN_PROGRESS") artifacts("look up")(error);
		console.log(JSON.stringify({ fork, step: "delete a fork stuck in FORK_IN_PROGRESS" }));
		const deleted = await deps.repos.delete(fork).catch(artifacts("delete the stuck Artifacts fork of"));
		if (!deleted)
			artifacts("delete the stuck Artifacts fork of")({ code: "NOT_DELETED", message: "the delete returned false" });
		exists = false;
	}
	if (exists) return;
	await deps.repos.create(fork).catch((error: unknown) => {
		// Made by a request racing this one: as good.
		if ((error as { code?: string }).code !== "ALREADY_EXISTS") artifacts("make")(error);
	});
}

/**
 * The copy's passwords: read on the tracker's repo, write on the fork,
 * recorded against the caller's token so revoking it revokes them.
 */
async function copyCredentials(
	env: Env,
	deps: Deps,
	caller: Caller,
	source: string,
	fork: string,
): Promise<Pick<CopyPlan, "from" | "to">> {
	const mint = async (name: string, scope: "read" | "write") => {
		const minted = await deps.repos.mint(name, scope, COPY_CREDENTIAL_SECONDS);
		await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
		return { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${minted.password}`)}` };
	};
	return { from: await mint(source, "read"), to: await mint(fork, "write") };
}

/**
 * Revoke repo passwords at Artifacts. Best effort per password: one already
 * gone (expired, or its repo deleted) must not stop the rest.
 */
/**
 * Make `name`'s fork of `tracker`, or find it made: its repo's name, or
 * "forking" while the copy into it runs. The caller has checked who may.
 */
async function makeFork(
	env: Env,
	deps: Deps,
	project: DurableObjectStub<ProjectDO>,
	id: ProjectId,
	key: string,
	caller: Caller,
	name: string,
	tracker: string,
): Promise<string | "forking"> {
	const fork = repoName({ kind: "fork", project: id, instance: name, tracker });
	const source = repoName({ kind: "tracker", project: id, tracker });
	// Whatever Artifacts says, in the answer: only its code and message
	// explain a failure on a large repository.
	const artifacts = (step: string) => (error: unknown) => {
		if (error instanceof HttpError) throw error;
		const { code, message } = error as { code?: string; message?: string };
		console.error(JSON.stringify({ fork, step, code, message }));
		throw new HttpError(
			502,
			`Artifacts could not ${step} \`${tracker}\`'s fork for \`${name}\`: ${code ?? "error"}: ${message ?? "no message"}`,
		);
	};

	// Complete forks are handed out as they are: the instance's work may be on them.
	if (await project.forkComplete(name, tracker)) {
		if ((await deps.repos.remote(fork).catch(artifacts("look up"))) !== null) return fork;
		// Its repo is gone; it is made and copied again below.
	}
	if (!deps.copy) throw new HttpError(501, "this remote has no Git endpoint to copy forks with");
	// The container copies the tracker's branches into an empty repo, in
	// the background: t3code's 365 MiB takes many minutes. The client asks
	// again on a 202 until the copy is done.
	const plan: CopyPlan = {
		dir: "/work",
		from: { url: deps.repos.url(source), auth: "" },
		to: { url: deps.repos.url(fork), auth: "" },
	};
	// Starting the copy makes the repo and its passwords; Artifacts'
	// refusals there are answered as they are, and anything else the copy
	// itself failed at, with Git's error as the container gave it.
	let refused: unknown;
	const answer = await deps
		.copy(key, plan, async () => {
			try {
				// Recorded before the repo exists, so removing the instance
				// deletes a fork whose copy never finished too.
				await project.startFork(name, tracker);
				await emptyFork(deps, fork, artifacts);
				return await copyCredentials(env, deps, caller, source, fork);
			} catch (error) {
				refused = error;
				throw error;
			}
		})
		.catch((error: unknown) => {
			if (refused !== undefined) throw refused;
			const why = error instanceof Error ? error.message : String(error);
			console.error(JSON.stringify({ fork, step: "copy", why }));
			throw new HttpError(502, `the remote could not copy \`${tracker}\` into \`${name}\`'s fork: ${why}`);
		});
	if (isChecking(answer)) return "forking";
	await project.completeFork(name, tracker);
	return fork;
}

/**
 * Land a merge into `branch` (the one compare-and-swap that is its commit
 * point), cancel its passwords, and have the repos' branches follow. What
 * could not follow is in `behind`; the landing stands regardless.
 */
export async function landAndFollow(
	deps: Deps,
	project: DurableObjectStub<ProjectDO>,
	id: ProjectId,
	branch: string,
	base: TrackerList,
	list: TrackerList,
	by: string,
	moves: RefMove[],
	background = false,
): Promise<{ ok: true; branch: Branch; behind: string[] } | { ok: false; reason: "missing" | "moved" }> {
	const landed = await project.land(branch, base, list, by);
	if (!landed.ok) return { ok: false, reason: landed.reason };
	await revokeRepoTokens(deps, landed.tokens);
	// The landing is the truth from here; the branches follow after the
	// answer, so the merger does not wait on them. What cannot follow is
	// logged, and the next landing (or a finish asked again) brings root's
	// branch along.
	if (background) {
		deps.waitUntil(
			followLanding(
				deps.repos,
				{
					head: (name, tracker) => project.head(name, tracker),
					wasHead: (name, tracker, commit) => project.wasHead(name, tracker, commit),
				},
				(tracker) => repoName({ kind: "tracker", project: id, tracker }),
				branch,
				moves,
			).then((behind) => {
				if (behind.length > 0)
					console.log(JSON.stringify({ landing: "behind", project: projectKey(id), branch, behind }));
			}),
		);
		return { ok: true, branch: (await project.branch(branch)) ?? landed.branch, behind: [] };
	}
	const behind = await followLanding(
		deps.repos,
		{
			head: (name, tracker) => project.head(name, tracker),
			wasHead: (name, tracker, commit) => project.wasHead(name, tracker, commit),
		},
		(tracker) => repoName({ kind: "tracker", project: id, tracker }),
		branch,
		moves,
	);
	if (behind.length > 0) console.log(JSON.stringify({ landing: "behind", project: projectKey(id), branch, behind }));
	return { ok: true, branch: (await project.branch(branch)) ?? landed.branch, behind };
}

/**
 * The branch moves a merge's finish asks for, each one the merge may make:
 * a tracker it was handed a write password for, its `main` (catching up) or
 * the branch merged into, and, there, the commit the index pins.
 */
function checkMoves(moves: unknown, writes: string[], branch: string, list: TrackerList): RefMove[] {
	if (moves === undefined) return [];
	if (!Array.isArray(moves)) throw new HttpError(400, "moves must be a list");
	const sha = /^[0-9a-f]{40}$/;
	return moves.map((move: Partial<RefMove>) => {
		const { tracker, branch: target, old, new: next } = move;
		if (typeof tracker !== "string" || typeof target !== "string" || typeof next !== "string" || !sha.test(next))
			throw new HttpError(400, "each move names a tracker, a branch, and a full commit id");
		if (old !== null && (typeof old !== "string" || !sha.test(old)))
			throw new HttpError(400, `\`${tracker}\`'s move must name a full commit id it moves from, or null`);
		if (tracker === "root") throw new HttpError(400, "root's branch follows the list; it is not asked for");
		if (!writes.includes(tracker))
			throw new HttpError(403, `this merge was not handed \`${tracker}\` to write, so it may not move it`);
		if (target !== "main" && target !== branch)
			throw new HttpError(400, `a merge into \`${branch}\` moves \`${tracker}\`'s \`main\` or \`${branch}\`, not \`${target}\``);
		if (target === branch && branch !== "main" && list[tracker] !== next)
			throw new HttpError(400, `\`${tracker}\`'s \`${branch}\` moves to the commit the index pins`);
		return { tracker, branch: target, old: old ?? null, new: next };
	});
}

function sameIndex(left: TrackerList | undefined, right: TrackerList): boolean {
	if (left === undefined) return false;
	const keys = Object.keys(left);
	return keys.length === Object.keys(right).length && keys.every((key) => left[key] === right[key]);
}

export async function revokeRepoTokens(deps: Deps, tokens: { repo: string; artifactsId: string }[]) {
	await Promise.allSettled(tokens.map((token) => deps.repos.revokeToken(token.repo, token.artifactsId)));
}

/** A private instance is there only for its owner (and agents acting as them). */
function visibleTo(instance: Instance, login: string): boolean {
	return !instance.private || instance.owner === login;
}

/** Instance tokens write only their own instance. */
function requireThisInstance(caller: Caller, instance: string) {
	if (caller.instance !== undefined && caller.instance !== instance)
		throw new HttpError(403, `this workspace's token is for instance \`${caller.instance}\` only`);
}

function requireNotInstance(caller: Caller, action: string) {
	if (caller.instance !== undefined)
		throw new HttpError(403, `a workspace's token cannot ${action}; run it from ${caller.login}'s own login`);
}

function requireHuman(caller: Caller, action: string) {
	if (caller.kind === "agent")
		throw new HttpError(403, `agent tokens cannot ${action}; ${caller.login} can, from their own login`);
}

export function projectStub(env: Env, id: ProjectId): DurableObjectStub<ProjectDO> {
	return env.PROJECTS.getByName(projectKey(id));
}

function requirePart(what: string, value: string) {
	const problem = partProblem(what, value);
	if (problem) throw new HttpError(400, problem);
}

function requireOwner(owner: string, login: string, action: string) {
	if (owner !== login) throw new HttpError(403, `only ${owner} may ${action}`);
}

async function body<T>(request: Request): Promise<T> {
	try {
		return (await request.json()) as T;
	} catch {
		throw new HttpError(400, "the request body must be JSON");
	}
}
