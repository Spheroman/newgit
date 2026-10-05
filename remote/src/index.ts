// The newgit remote's API. Every route but login takes `Authorization: Bearer
// ngt_…` and answers JSON; errors are `{ error }` with a status that says
// whose problem it is. Built for the CLI (and so for agents): the web UI uses
// the same routes.

import { parse as parseToml } from "smol-toml";

import { type Repos, ArtifactsRepos } from "./artifacts";
import {
	AGENT_TTL_MAX_SECONDS,
	AGENT_TTL_SECONDS,
	LOGIN_TTL_SECONDS,
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
	parseProjectKey,
	parseRepoName,
	partProblem,
	projectKey,
	repoName,
} from "./names";
import type { ProjectDO } from "./project";
import type { UserDO } from "./users";

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

export interface Deps {
	repos: Repos;
	fetcher: typeof fetch;
	/** Work that may outlive the response (a fork takes seconds). */
	waitUntil(promise: Promise<unknown>): void;
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
		return handle(request, env, {
			repos: new ArtifactsRepos(env.ARTIFACTS, env.ARTIFACTS_GIT_BASE),
			fetcher: fetch,
			waitUntil: (promise) => ctx.waitUntil(promise),
		});
	},
} satisfies ExportedHandler<Env>;

export async function handle(request: Request, env: Env, deps: Deps): Promise<Response> {
	try {
		return await route(request, env, deps);
	} catch (error) {
		if (error instanceof HttpError) return json({ error: error.message }, error.status);
		console.error(error);
		return json({ error: "internal error" }, 500);
	}
}

async function route(request: Request, env: Env, deps: Deps): Promise<Response> {
	const url = new URL(request.url);
	const path = url.pathname.replace(/\/+$/, "");
	const method = request.method;

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
			throw new HttpError(400, "trackers is required: the code trackers whose repos to create (root, project, …)");
		for (const tracker of trackers) requirePart("tracker name", tracker);
		const id: ProjectId = { owner: login, name };
		const project = projectStub(env, id);
		if (!(await project.init(name, login)))
			throw new HttpError(409, `project \`${projectKey(id)}\` already exists`);
		// The one write to a code tracker's repo outside a merge: the creator's
		// initial push of its history. Only here, only once, only briefly.
		const seeds = [];
		for (const tracker of trackers) seeds.push(await seed(env, deps, caller, { kind: "tracker", project: id, tracker }));
		return json(
			{ owner: login, name, key: projectKey(id), endpoint: endpointUrl(url, id), seeds },
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

	// ── Code trackers ──────────────────────────────────────────────────────
	// A code tracker carved after the project was created gets its repo here,
	// seeded once by whoever carved it. It joins a branch only by a merge.
	if (method === "POST" && rest === "/trackers") {
		requireHuman(caller, "create code tracker repos");
		const { name: tracker } = await body<{ name?: string }>(request);
		if (!tracker) throw new HttpError(400, "name is required");
		requirePart("tracker name", tracker);
		const ref: RepoRef = { kind: "tracker", project: id, tracker };
		if ((await deps.repos.remote(repoName(ref))) !== null)
			throw new HttpError(409, `${repoName(ref)} exists already; its history moves only through \`newgit merge\``);
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
	const branchMatch = rest.match(/^\/branches\/([^/]+)$/);
	if (branchMatch) {
		const branch = decodeURIComponent(branchMatch[1]);
		if (method === "GET") {
			const found = await project.branch(branch);
			if (found === null) throw new HttpError(404, `no branch \`${branch}\` in ${key}`);
			return json(found);
		}
		if (method === "PUT") {
			requireNotInstance(caller, "create branches");
			requirePart("branch name", branch);
			const { list } = await body<{ list?: Record<string, string> }>(request);
			if (!list || typeof list !== "object" || Object.keys(list).length === 0)
				throw new HttpError(400, "list is required: one commit per code tracker, by tracker name");
			for (const [tracker, commit] of Object.entries(list)) {
				requirePart("tracker name", tracker);
				if (typeof commit !== "string" || !/^[0-9a-f]{40}$/.test(commit))
					throw new HttpError(400, `\`${tracker}\` must name a full commit id`);
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
		const { name, base_rev } = await body<{ name?: string; base_rev?: string }>(request);
		if (!name || !base_rev) throw new HttpError(400, "name and base_rev are required");
		requirePart("instance name", name);
		const created = await project.createInstance(name, login, base_rev, caller.agent ?? null);
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
	if (method === "GET" && rest === "/instances") return json({ instances: await project.instances() });

	const forkMatch = rest.match(/^\/instances\/([^/]+)\/trackers\/([^/]+)\/fork$/);
	if (method === "POST" && forkMatch) {
		const name = decodeURIComponent(forkMatch[1]);
		const tracker = decodeURIComponent(forkMatch[2]);
		const instance = await project.instance(name);
		if (instance === null) throw new HttpError(404, `no instance \`${name}\` in ${key}`);
		requireOwner(instance.owner, login, `fork for \`${name}\``);
		requireThisInstance(caller, name);
		const facts = await definitionFacts(deps.repos, id, info.defaultBranch, tracker);
		if (facts === undefined) throw new HttpError(404, `tracker \`${tracker}\` is not defined on ${key}'s default branch`);
		if (!facts.code) throw new HttpError(400, `\`${tracker}\` is a secret tracker; instances do not fork it`);
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
		if (instance === null) throw new HttpError(404, `no instance \`${name}\` in ${key}`);
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
		return json({ checkpoints: await project.checkpoints(instance) });
	}
	const checkpointMatch = rest.match(/^\/checkpoints\/([^/]+)\/([^/]+)$/);
	if (checkpointMatch) {
		const instance = decodeURIComponent(checkpointMatch[1]);
		const checkpoint = decodeURIComponent(checkpointMatch[2]);
		if (method === "GET") {
			const found = await project.checkpoint(instance, checkpoint);
			if (found === null) throw new HttpError(404, `no checkpoint ${instance}/${checkpoint} in ${key}`);
			return json(found);
		}
		if (method === "PUT") {
			const owner = (await project.instance(instance))?.owner;
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

/** Where a project's Git endpoint is: `https://<host>/<owner>/<name>.git`. */
function endpointUrl(url: URL, id: ProjectId): string {
	return `${url.origin}/${id.owner}/${id.name}.git`;
}

/** Create a code tracker's repo and mint the one short write that seeds it. */
async function seed(env: Env, deps: Deps, caller: Caller, ref: RepoRef) {
	const name = repoName(ref);
	const { remote } = await deps.repos.create(name);
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
	if (ref.kind === "fork") facts.instanceOwner = (await project.instance(ref.instance))?.owner;
	facts.tracker = await definitionFacts(deps.repos, ref.project, defaultBranch, ref.tracker);

	const decision = decide(ref, scope, who, facts);
	if (!decision.ok) throw new HttpError(403, decision.reason);

	const name = repoName(ref);
	let remote = await deps.repos.remote(name);
	if (remote === null) {
		// A secret tracker's repo is created on its first write. Code tracker
		// repos are seeded with the project, and forks made by the fork route.
		if (ref.kind !== "tracker" || scope !== "write" || facts.tracker?.code)
			throw new HttpError(404, `${name} does not exist yet`);
		remote = (await deps.repos.create(name)).remote;
	}
	const minted = await deps.repos.mint(name, scope, CREDENTIAL_TTL_SECONDS);
	await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
	return { repo: name, remote, scope, username: "x", password: minted.password, expires_at: minted.expiresAt };
}

/**
 * A tracker's definition — audience, and whether it is a code tracker — as
 * committed on the project's default branch: the `project` tracker's
 * `.newgit/trackers/<name>.toml` there. Read from the branch, not taken from
 * the caller: a definition changes only by merging a change to it.
 */
async function definitionFacts(
	repos: Repos,
	project: ProjectId,
	branch: string,
	tracker: string,
): Promise<{ audience: Audience; code: boolean } | undefined> {
	const text = await repos.readFile(
		repoName({ kind: "tracker", project, tracker: "project" }),
		branch,
		`.newgit/trackers/${tracker}.toml`,
	);
	if (text === null) return undefined;
	let definition: { audience?: unknown; secret?: unknown };
	try {
		definition = parseToml(text) as { audience?: unknown; secret?: unknown };
	} catch {
		return undefined;
	}
	const audience = typeof definition.audience === "string" ? parseAudience(definition.audience) : null;
	if (audience === null) return undefined;
	// `secret` defaults to true, and is written only when false.
	return { audience, code: definition.secret === false };
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
	const claims = token ? await verifyToken(env.TOKEN_KEY, token) : null;
	if (claims === null) throw new HttpError(401, "not logged in: run `newgit login`");
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
