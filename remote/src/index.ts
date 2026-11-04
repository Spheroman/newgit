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
import { type AncestryAnswer, type AncestryPlan, type EndpointKind, type EndpointPlan, type IndexAnswer, type IndexPlan, PLAN_HEADER } from "./endpoint";
import type { Instance, ProjectDO } from "./project";
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

/** A merge holds its branch this long at most: pushing the trackers' commits. */
const MERGE_LOCK_SECONDS = 10 * 60;

export interface Deps {
	repos: Repos;
	fetcher: typeof fetch;
	/** Work that may outlive the response (a fork takes seconds). */
	waitUntil(promise: Promise<unknown>): void;
	/** Serve one Git request from a project's endpoint container. */
	endpoint(project: string, kind: EndpointKind, plan: EndpointPlan, request: Request): Promise<Response>;
	/**
	 * Ask the project's endpoint container, which runs Git, how each pin
	 * moves along its tracker's history. Absent (tests that do not fake it),
	 * every move counts as not forward.
	 */
	ancestry?(project: string, plan: AncestryPlan): Promise<AncestryAnswer[]>;
	/**
	 * Ask the container whether an index is what its root commit pins. The
	 * production Deps always has it; a test that does not fake it skips the
	 * check.
	 */
	index?(project: string, plan: IndexPlan): Promise<IndexAnswer>;
}

class HttpError extends Error {
	constructor(
		readonly status: number,
		message: string,
	) {
		super(message);
	}
}

const json = (body: unknown, status = 200) => Response.json(body, { status });

export default {
	async fetch(request, env, ctx): Promise<Response> {
		// The landing page is read from a tracker on every miss, so the edge
		// keeps it for as long as it says it may (`landingPage`).
		const landing = request.method === "GET" && new URL(request.url).pathname === "/";
		if (landing) {
			const cached = await caches.default.match(request);
			if (cached) return cached;
		}
		const response = await handle(request, env, {
			repos: new ArtifactsRepos(env.ARTIFACTS, env.ARTIFACTS_GIT_BASE),
			fetcher: fetch,
			waitUntil: (promise) => ctx.waitUntil(promise),
			ancestry: async (project, plan) => {
				const response = await env.ENDPOINTS.getByName(project).fetch(
					new Request("https://endpoint/ancestry", {
						method: "POST",
						headers: { [PLAN_HEADER]: btoa(JSON.stringify(plan)) },
					}),
				);
				if (!response.ok) throw new HttpError(502, "the remote could not check where the merge's pins move; try again");
				return (await response.json()) as AncestryAnswer[];
			},
			index: async (project, plan) => {
				const response = await env.ENDPOINTS.getByName(project).fetch(
					new Request("https://endpoint/index", {
						method: "POST",
						headers: { [PLAN_HEADER]: btoa(JSON.stringify(plan)) },
					}),
				);
				if (!response.ok) throw new HttpError(502, "the remote could not check the merge's index; try again");
				return (await response.json()) as IndexAnswer;
			},
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
		});
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
		const { repo, scope = "read" } = await body<{ repo?: string; scope?: "read" | "write" }>(request);
		if (!repo) throw new HttpError(400, "repo is required (a repo name, or a remote URL)");
		if (scope !== "read" && scope !== "write") throw new HttpError(400, "scope is read or write");
		const ref = parseRepoName(repoFromUrlOrName(repo));
		if (ref === null || projectKey(ref.project) !== key)
			throw new HttpError(400, `\`${repo}\` is not one of ${key}'s repos`);
		if (caller.instance !== undefined && scope === "write" && ref.kind === "fork")
			requireThisInstance(caller, ref.instance);
		return json(await credential(env, deps, project, info.defaultBranch, ref, scope, caller, member));
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
		const defined = await definitionFacts(deps.repos, id, info.defaultBranch, tracker, await listedTrackers(project));
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
		// maintainers' to make, and a commit off `main` means its `main` moves
		// too, so they get a write password for it. Without the container
		// (a test, or one that cannot start), every move counts as not
		// forward: the safe answer.
		const children = trackers.filter((tracker) => tracker !== "root" && tracker in base && tracker in list);
		const moves = new Map<string, AncestryAnswer>();
		if (children.length > 0 && deps.ancestry) {
			const plan: AncestryPlan = { dir: "/work", repos: {}, checks: [] };
			for (const tracker of children) {
				const name = repoName({ kind: "tracker", project: id, tracker });
				const minted = await deps.repos.mint(name, "read", CREDENTIAL_TTL_SECONDS);
				await userStub(env, login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
				plan.repos[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${minted.password}`)}` };
				plan.checks.push({ tracker, old: base[tracker], new: list[tracker] });
			}
			for (const answer of await deps.ancestry(key, plan)) moves.set(answer.tracker, answer);
		}
		// Everything is checked before the lock is taken: a refused merge
		// holds nothing.
		const who = { login, member };
		const writes: string[] = [];
		for (const tracker of trackers) {
			const facts = await definitionFacts(deps.repos, id, info.defaultBranch, tracker, await listedTrackers(project));
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
					`\`${tracker}\` is not defined on \`${info.defaultBranch}\`: a new tracker in Git joins by its own seeded repo`,
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
		const started = await project.beginMerge(branch, base, login, MERGE_LOCK_SECONDS, trackers, list);
		if (!started.ok) {
			if (started.reason === "missing") throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
			if (started.reason === "moved")
				throw new HttpError(409, `\`${branch}\` moved since you looked: merge again onto what it is now`);
			throw new HttpError(409, `${started.by} is merging into \`${branch}\` until ${started.until}`);
		}
		await revokeRepoTokens(deps, started.stale);
		const credentials = [];
		const handed = [];
		for (const tracker of writes) {
			const ref: RepoRef = { kind: "tracker", project: id, tracker };
			const name = repoName(ref);
			if ((await deps.repos.remote(name)) === null) throw new HttpError(404, `${name} does not exist`);
			// No longer-lived than the lock, and cancelled when the merge ends.
			const minted = await deps.repos.mint(name, "write", MERGE_LOCK_SECONDS);
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
		await project.recordMergeTokens(branch, handed);
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
			const { list, base } = await body<{ list?: Record<string, string>; base?: Record<string, string> }>(
				request,
			);
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
				if (!(await project.mergeTargets(branch, list)))
					throw new HttpError(400, "this is not the index the merge began with: start the merge again");
				// And what root's pushed commit really pins: the Git endpoint
				// composes from the gitlinks, so an index that says otherwise
				// would have had its pins checked for nothing.
				if (deps.index) {
					const plan: IndexPlan = { dir: "/work", repos: {}, list };
					for (const tracker of Object.keys(list)) {
						const name = repoName({ kind: "tracker", project: id, tracker });
						const minted = await deps.repos.mint(name, "read", CREDENTIAL_TTL_SECONDS);
						await userStub(env, login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
						plan.repos[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${minted.password}`)}` };
					}
					const checked = await deps.index(key, plan);
					if (!checked.matches)
						throw new HttpError(
							400,
							"root's commit on the branch does not pin what this index says: push the root commit the merge made, then finish",
						);
				}
				const finished = await project.finishMerge(branch, base, list, login);
				if (!finished.ok) {
					if (finished.reason === "missing") throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
					if (finished.reason === "unlocked")
						throw new HttpError(409, `you hold no merge into \`${branch}\`: start one with \`newgit merge\``);
					throw new HttpError(409, `\`${branch}\` moved since you looked: merge again onto what it is now`);
				}
				await revokeRepoTokens(deps, finished.tokens);
				return json(finished.branch);
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
		// Nothing is forked here: an instance forks a tracker the first time
		// it changes it. The workspace's own credential is minted here so
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
		const facts = await definitionFacts(deps.repos, id, info.defaultBranch, tracker, await listedTrackers(project));
		if (facts === undefined) throw new HttpError(404, `tracker \`${tracker}\` is not defined on ${key}'s default branch`);
		if (!facts.git) throw new HttpError(400, `\`${tracker}\` is not in Git; instances do not fork it`);
		const fork = repoName({ kind: "fork", project: id, instance: name, tracker });
		let remote = await deps.repos.remote(fork);
		if (remote === null) {
			// Seconds on a real repo; the request waits, since the push that
			// asked for it cannot go anywhere until it exists.
			remote = (await deps.repos.fork(repoName({ kind: "tracker", project: id, tracker }), fork)).remote;
		}
		await project.addFork(name, tracker);
		return json({ instance: name, tracker, repo: fork, remote });
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
			for (const tracker of instance.forks)
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
			const { record } = await body<{ record?: unknown }>(request);
			if (record === undefined) throw new HttpError(400, "record is required");
			return json(await project.putCheckpoint(instance, checkpoint, login, caller.agent ?? null, record), 201);
		}
	}

	throw new HttpError(404, `no route for ${method} ${path}`);
}

// ── The Git endpoint ──────────────────────────────────────────────────────

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
	const caller = await callerFor(token, env);

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
	const info = await project.info();
	if (info === null) throw new HttpError(404, `no project \`${key}\``);
	const who = { login: caller.login, member: await project.isMember(caller.login) };

	const branches = await project.branches();
	const main = branches.find((branch) => branch.name === info.defaultBranch)?.list;
	if (!main?.root) throw new HttpError(404, `${key} has no \`${info.defaultBranch}\` branch yet`);

	// Policy, read as the remote reads it: each definition from the tracker
	// that owns its file, at the default branch. Shape (pins, mounts) is the
	// container's to read from the commits themselves.
	const facts = await loadDefinitions(
		deps.repos,
		id,
		info.defaultBranch,
		branches.flatMap((branch) => Object.keys(branch.list)),
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
		const minted = await deps.repos.mint(name, "read", CREDENTIAL_TTL_SECONDS);
		await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
		plan.repos[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${minted.password}`)}` };
	}
	return deps.endpoint(key, service === match[3] ? "upload-pack" : "advertise", plan, request);
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
	defaultBranch: string,
	ref: RepoRef,
	scope: "read" | "write",
	caller: Caller,
	member: boolean,
) {
	const who = { login: caller.login, member };
	const facts: Facts = {};
	if (ref.kind === "fork") {
		const instance = await project.instance(ref.instance);
		facts.instanceOwner = instance?.owner;
		facts.instancePrivate = instance?.private;
	}
	facts.tracker = await definitionFacts(deps.repos, ref.project, defaultBranch, ref.tracker, await listedTrackers(project));
	// Not a refusal but an absence, as on the fork route: a client may wait
	// for the definition to land (a checkpoint leaves the tracker for later),
	// while a 403 is a "no" it must not wait out.
	if (facts.tracker === undefined)
		throw new HttpError(404, `tracker \`${ref.tracker}\` is not defined on ${projectKey(ref.project)}'s default branch`);

	const decision = decide(ref, scope, who, facts);
	if (!decision.ok) throw new HttpError(403, decision.reason);

	const name = repoName(ref);
	let remote = await deps.repos.remote(name);
	if (remote === null) {
		// The repo of a tracker outside Git is created on its first write. The repos of
		// trackers in Git are seeded with the project, and forks made by the fork route.
		if (ref.kind !== "tracker" || scope !== "write" || facts.tracker?.git)
			throw new HttpError(404, `${name} does not exist yet`);
		remote = (await deps.repos.create(name)).remote;
	}
	const minted = await deps.repos.mint(name, scope, CREDENTIAL_TTL_SECONDS);
	await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
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
 * Every named tracker's committed definition on the project's default
 * branch. A definition is the view's `.newgit/trackers/<name>.toml`: `access`'s
 * `<name>.toml`, unless a tracker in Git owns that path by the ownership rules
 * — which is how governance layers: changing a tracker's definition is
 * changing the tracker that owns its file, and only that tracker's
 * maintainers merge it. The file is read from its owner's repo, relative to
 * the owner's mount. Resolved to a fixed point, a few reads per round.
 */
async function loadDefinitions(
	repos: Repos,
	project: ProjectId,
	branch: string,
	names: Iterable<string>,
): Promise<Map<string, TrackerFacts>> {
	const all = new Set<string>(["root", "access", ...names]);
	const file = (name: string) => `${ACCESS_MOUNT}/${name}.toml`;
	const source = new Map<string, { tracker: string; mount: string }>(
		[...all].map((name) => [name, { tracker: "access", mount: ACCESS_MOUNT }]),
	);
	let facts = new Map<string, TrackerFacts>();
	for (let round = 0; round < 4; round++) {
		facts = new Map();
		for (const name of all) {
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
			const read = text === null ? implicitFacts(name) : parseFacts(text, from.tracker);
			if (read !== undefined) facts.set(name, { ...read, source: from.tracker });
		}
		let moved = false;
		for (const name of all) {
			const owner = ownerOf(facts, file(name)) ?? "access";
			const mount = facts.get(owner)?.mount ?? ACCESS_MOUNT;
			const was = source.get(name)!;
			if (was.tracker !== owner || was.mount !== mount) {
				source.set(name, { tracker: owner, mount });
				moved = true;
			}
		}
		if (!moved) return facts;
	}
	return facts;
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
	const definitions = await loadDefinitions(deps.repos, id, info.defaultBranch, [tracker, ...listed]);
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
	branch: string,
	tracker: string,
	listed: Iterable<string> = [],
): Promise<TrackerFacts | undefined> {
	return (await loadDefinitions(repos, project, branch, [tracker, ...listed])).get(tracker);
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

/**
 * Revoke repo passwords at Artifacts. Best effort per password: one already
 * gone (expired, or its repo deleted) must not stop the rest.
 */
async function revokeRepoTokens(deps: Deps, tokens: { repo: string; artifactsId: string }[]) {
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

function projectStub(env: Env, id: ProjectId): DurableObjectStub<ProjectDO> {
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
