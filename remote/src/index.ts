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
import { type Audience, type RepoRef, decide, parseAudience, parseRepoName, partProblem, repoName } from "./names";
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
		if (project !== undefined) requirePart("project name", project);
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
			const alive = await projectStub(env, caller.project).instance(caller.instance);
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
		const { name } = await body<{ name?: string }>(request);
		if (!name) throw new HttpError(400, "name is required");
		requirePart("project name", name);
		const project = projectStub(env, name);
		if (!(await project.init(name, login))) throw new HttpError(409, `project \`${name}\` already exists`);
		const src = repoName({ kind: "src", project: name });
		const { remote } = await deps.repos.create(src);
		// The one write to trunk outside a merge: the creator's initial push of
		// the project's existing history. Only here, only once, only briefly.
		const seed = await deps.repos.mint(src, "write", CREDENTIAL_TTL_SECONDS);
		return json(
			{
				name,
				owner: login,
				src: { repo: src, remote },
				seed_credential: { username: "x", password: seed.password, expires_at: seed.expiresAt },
			},
			201,
		);
	}

	const projectMatch = path.match(/^\/v1\/projects\/([^/]+)(\/.*)?$/);
	if (!projectMatch) throw new HttpError(404, `no route for ${method} ${path}`);
	const projectName = decodeURIComponent(projectMatch[1]);
	const rest = projectMatch[2] ?? "";
	if (caller.project !== undefined && caller.project !== projectName)
		throw new HttpError(403, `this agent token (\`${caller.agent}\`) is for ${caller.project} only`);
	const project = projectStub(env, projectName);
	const info = await project.info();
	if (info === null) throw new HttpError(404, `no project \`${projectName}\``);
	const member = await project.isMember(login);

	// Credentials are answered for non-members too: a public tracker is
	// readable by anyone logged in, and a refusal should say why.
	if (method === "POST" && rest === "/credentials") {
		const { repo, scope = "read" } = await body<{ repo?: string; scope?: "read" | "write" }>(request);
		if (!repo) throw new HttpError(400, "repo is required (a repo name, or a remote URL)");
		if (scope !== "read" && scope !== "write") throw new HttpError(400, "scope is read or write");
		const ref = parseRepoName(repoFromUrlOrName(repo));
		if (ref === null || ref.project !== projectName)
			throw new HttpError(400, `\`${repo}\` is not one of ${projectName}'s repos`);
		if (caller.instance !== undefined && scope === "write" && ref.kind === "instance")
			requireThisInstance(caller, ref.instance);
		return json(await credential(env, deps, project, ref, scope, caller, member));
	}

	if (!member) throw new HttpError(403, `${login} is not a member of ${projectName}`);

	if (method === "GET" && rest === "") {
		const src = repoName({ kind: "src", project: projectName });
		return json({
			...info,
			members: await project.members(),
			src: { repo: src, remote: await deps.repos.remote(src) },
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

	// ── Instances ──────────────────────────────────────────────────────────
	if (method === "POST" && rest === "/instances") {
		requireNotInstance(caller, "create instances");
		const { name, base_rev } = await body<{ name?: string; base_rev?: string }>(request);
		if (!name || !base_rev) throw new HttpError(400, "name and base_rev are required");
		requirePart("instance name", name);
		const created = await project.createInstance(name, login, base_rev, caller.agent ?? null);
		if (created === null) throw new HttpError(409, `instance \`${name}\` already exists in ${projectName}`);
		// A fork takes seconds; the record exists now and says the fork is pending.
		const src = repoName({ kind: "src", project: projectName });
		const fork = repoName({ kind: "instance", project: projectName, instance: name });
		deps.waitUntil(
			deps.repos.fork(src, fork).then(
				() => project.setForkState(name, "ready"),
				(error: unknown) => project.setForkState(name, "failed", String(error)),
			),
		);
		// The workspace's own credential, minted here so nobody has to: it can
		// write this instance and nothing else, renews itself, and is revoked
		// when the instance is removed.
		const instanceToken = await issue(env, {
			kind: "agent",
			login,
			agent: caller.agent ?? name,
			project: projectName,
			instance: name,
			ttlSeconds: AGENT_TTL_SECONDS,
		});
		return json(
			{
				...created,
				repo: fork,
				remote: deps.repos.url(fork),
				trunk: { repo: src, remote: deps.repos.url(src) },
				token: instanceToken.token,
				token_id: instanceToken.id,
			},
			201,
		);
	}
	if (method === "GET" && rest === "/instances") return json({ instances: await project.instances() });

	const instanceMatch = rest.match(/^\/instances\/([^/]+)(\/head)?$/);
	if (instanceMatch) {
		const name = decodeURIComponent(instanceMatch[1]);
		const instance = await project.instance(name);
		if (instance === null) throw new HttpError(404, `no instance \`${name}\` in ${projectName}`);
		const repo = repoName({ kind: "instance", project: projectName, instance: name });
		if (method === "GET" && !instanceMatch[2]) return json({ ...instance, repo, remote: deps.repos.url(repo) });
		if (method === "PUT" && instanceMatch[2]) {
			requireOwner(instance.owner, login, `move \`${name}\`'s head`);
			requireThisInstance(caller, name);
			const { head_rev } = await body<{ head_rev?: string }>(request);
			if (!head_rev) throw new HttpError(400, "head_rev is required");
			return json({ ...(await project.setHead(name, head_rev)), repo });
		}
		if (method === "DELETE" && !instanceMatch[2]) {
			requireOwner(instance.owner, login, `remove \`${name}\``);
			requireNotInstance(caller, "remove instances");
			await deps.repos.delete(repo).catch(() => false);
			await project.deleteInstance(name);
			await revokeRepoTokens(deps, await userStub(env, instance.owner).revokeInstance(projectName, name));
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
		const id = decodeURIComponent(checkpointMatch[2]);
		if (method === "GET") {
			const found = await project.checkpoint(instance, id);
			if (found === null) throw new HttpError(404, `no checkpoint ${instance}/${id} in ${projectName}`);
			return json(found);
		}
		if (method === "PUT") {
			const owner = (await project.instance(instance))?.owner;
			if (owner === undefined) throw new HttpError(404, `no instance \`${instance}\` in ${projectName}`);
			requireOwner(owner, login, `checkpoint \`${instance}\``);
			requireThisInstance(caller, instance);
			const { record } = await body<{ record?: unknown }>(request);
			if (record === undefined) throw new HttpError(400, "record is required");
			return json(await project.putCheckpoint(instance, id, login, caller.agent ?? null, record), 201);
		}
	}

	throw new HttpError(404, `no route for ${method} ${path}`);
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
	const facts: { instanceOwner?: string; audience?: Audience } = {};
	if (ref.kind === "instance") facts.instanceOwner = (await project.instance(ref.instance))?.owner;
	if (ref.kind === "tracker") facts.audience = await trunkAudience(deps.repos, ref.project, ref.tracker);

	const decision = decide(ref, scope, who, facts);
	if (!decision.ok) throw new HttpError(403, decision.reason);

	const name = repoName(ref);
	let remote = await deps.repos.remote(name);
	if (remote === null) {
		// Tracker repos are created on first write; nothing else is created here.
		if (ref.kind !== "tracker" || scope !== "write") throw new HttpError(404, `${name} does not exist yet`);
		remote = (await deps.repos.create(name)).remote;
	}
	const minted = await deps.repos.mint(name, scope, CREDENTIAL_TTL_SECONDS);
	await userStub(env, caller.login).recordRepoToken(caller.tokenId, name, minted.id, minted.expiresAt);
	return { repo: name, remote, scope, username: "x", password: minted.password, expires_at: minted.expiresAt };
}

/**
 * A tracker's audience as committed on trunk. Read from trunk, not taken from
 * the caller: an audience changes only by merging a change to its definition,
 * and only the merge executor writes trunk.
 */
async function trunkAudience(repos: Repos, project: string, tracker: string): Promise<Audience | undefined> {
	const text = await repos.readFile(
		repoName({ kind: "src", project }),
		"HEAD",
		`.newgit/trackers/${tracker}.toml`,
	);
	if (text === null) return undefined;
	let definition: { audience?: unknown };
	try {
		definition = parseToml(text) as { audience?: unknown };
	} catch {
		return undefined;
	}
	return typeof definition.audience === "string" ? (parseAudience(definition.audience) ?? undefined) : undefined;
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

function projectStub(env: Env, name: string): DurableObjectStub<ProjectDO> {
	return env.PROJECTS.getByName(name);
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
