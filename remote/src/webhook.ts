// GitHub's push webhook, which brings merged pull requests into newgit.dev
// (docs/design/remote.md, *The GitHub webhook*). GitHub expects a quick
// answer and the work takes as long as the container does, so the route only
// verifies the event and queues it on the project; `advancePulls` then moves
// the queue on, called from the route and from `ProjectDO`'s alarm, which is
// also what keeps the endpoint awake while its pull runs: nobody else is
// polling it.
//
// A pull lands as a merge does. It takes `main`'s merge lock, the container
// moves root's ref by compare-and-swap, and the index moves last. If someone
// else holds the lock it waits and tries again. Every step can run twice: a
// redelivery, an evicted object, or a retry finds the work done and ends.

import { parse as parseToml } from "smol-toml";

import { type Deps, HttpError, isChecking, loadDefinitions, projectStub, revokeRepoTokens } from "./index";
import { pausedBy } from "./budget";
import type { PullPlan } from "./endpoint";
import { decide, mayMaintain, type ProjectId, projectKey, repoName } from "./names";

/** Who holds `main`'s merge lock during a pull. Not a login: logins have no spaces. */
export const WEBHOOK_MERGER = "the GitHub webhook";

/** The branch a mirror's main branch stands for, as an export pushes it. */
export const PULL_BRANCH = "main";

/** How often the alarm looks at a pull: a poll of the container, or of a lock. */
export const PULL_POLL_MS = 15_000;

/** A pull holds the lock this long past each poll, so a stalled one lets go. */
export const PULL_LOCK_SECONDS = 20 * 60;

/** Past this, a pull waiting on a lock gives up; a redelivery starts it again. */
export const PULL_LIMIT_MS = 2 * 60 * 60_000;

/** How many times a failure is retried before a redelivery has to. */
export const PULL_ATTEMPTS = 5;

/** The passwords a pull's container fetches and pushes with: past `CHECK_LIMIT_MS`. */
const PULL_CREDENTIAL_SECONDS = 50 * 60;

/** One verified event, waiting for or being worked on by the container. */
export interface MirrorPull {
	target: string;
	/** `owner/name` on GitHub, as the signed event gives it. */
	repo: string;
	branch: string;
	commit: string;
	/** Who pushed, by GitHub login: the login newgit knows them by. */
	pusher: string;
	/** The trackers this pull may change: what the pusher maintains. */
	writable: string[];
	queuedAt: number;
	attempts: number;
	/** `main`'s index when this pull took the lock; unset until then. */
	base?: Record<string, string>;
}

/**
 * Whether `header` is GitHub's signature of `body`: `sha256=` and the
 * HMAC-SHA256 of the raw bytes. `verify` compares in constant time.
 */
export async function verifySignature(secret: string, body: ArrayBuffer, header: string | null): Promise<boolean> {
	const hex = header?.match(/^sha256=([0-9a-f]{64})$/)?.[1];
	if (hex === undefined) return false;
	const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
		"verify",
	]);
	const signature = Uint8Array.from(hex.match(/../g) ?? [], (pair) => Number.parseInt(pair, 16));
	return crypto.subtle.verify("HMAC", key, signature, body);
}

/** The `owner/name` an export target's `url` is on GitHub, lowercased; null when it is elsewhere. */
export function githubRepo(url: string): string | null {
	const found =
		url.match(/^(\w[\w.-]*\/\w[\w.-]*)$/) ??
		url.match(/^https:\/\/github\.com\/([\w.-]+\/[\w.-]+?)(?:\.git)?\/?$/) ??
		url.match(/^git@github\.com:([\w.-]+\/[\w.-]+?)(?:\.git)?$/);
	return found ? found[1].toLowerCase() : null;
}

interface Target {
	url: string;
	branch: string;
	root?: string;
	trackers: string[];
}

function parseTarget(text: string): Target | null {
	let file: { url?: unknown; branch?: unknown; root?: unknown; trackers?: unknown };
	try {
		file = parseToml(text) as typeof file;
	} catch {
		return null;
	}
	if (typeof file.url !== "string") return null;
	return {
		url: file.url,
		branch: typeof file.branch === "string" ? file.branch : "main",
		...(typeof file.root === "string" ? { root: file.root } : {}),
		trackers: Array.isArray(file.trackers) ? file.trackers.filter((name) => typeof name === "string") : [],
	};
}

const refused = (status: number, reason: string, context: Record<string, unknown>): never => {
	console.error(JSON.stringify({ webhook: "refused", reason, ...context }));
	throw new HttpError(status, reason);
};

/**
 * `POST /v1/webhooks/github?project=<owner>/<name>&target=<target>`. Which
 * project and target an event is for is in the URL, set when the webhook was
 * made; the project's own `.newgit/exports/<target>.toml` then has to name
 * the repo the event came from, so one repo's event cannot move a project
 * that does not say it mirrors there.
 */
export async function githubWebhook(request: Request, env: Env, deps: Deps): Promise<Response> {
	if (!env.GITHUB_WEBHOOK_SECRET) throw new HttpError(503, "this remote has no GITHUB_WEBHOOK_SECRET");
	const raw = await request.arrayBuffer();
	if (!(await verifySignature(env.GITHUB_WEBHOOK_SECRET, raw, request.headers.get("x-hub-signature-256"))))
		throw new HttpError(401, "bad signature");

	const event = request.headers.get("x-github-event");
	if (event === "ping") return Response.json({ pong: true });
	if (event !== "push") return Response.json({ ignored: `${event} events are not used` });

	let payload: {
		ref?: string;
		after?: string;
		deleted?: boolean;
		repository?: { full_name?: string; private?: boolean };
		pusher?: { name?: string };
	};
	try {
		payload = JSON.parse(new TextDecoder().decode(raw));
	} catch {
		throw new HttpError(400, "the event is not JSON");
	}
	const url = new URL(request.url);
	const context = { project: url.searchParams.get("project"), target: url.searchParams.get("target"), repo: payload.repository?.full_name };
	const id = parseProject(context.project);
	if (id === null) return refused(400, "the webhook's URL needs `?project=<owner>/<name>`", context);
	const target = context.target;
	if (target === null || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(target))
		return refused(400, "the webhook's URL needs `&target=<export target>`", context);
	const project = projectStub(env, id);
	const info = await project.info();
	if (info === null) return refused(404, `no project \`${projectKey(id)}\``, context);

	const text = await deps.repos.readFile(
		repoName({ kind: "tracker", project: id, tracker: "root" }),
		"main",
		`.newgit/exports/${target}.toml`,
	);
	if (text === null) return refused(404, `root's \`main\` has no export target \`${target}\``, context);
	const definition = parseTarget(text);
	if (definition === null) return refused(400, `export target \`${target}\` cannot be read`, context);
	const repo = payload.repository?.full_name ?? "";
	if (githubRepo(definition.url) !== repo.toLowerCase())
		return refused(403, `export target \`${target}\` does not mirror to ${repo}`, context);

	if (definition.root !== undefined)
		return Response.json({ ignored: "a rooted target's commits are its tracker's: `newgit pull --remote`" });
	if (payload.ref !== `refs/heads/${definition.branch}`)
		return Response.json({ ignored: `not \`${definition.branch}\`` });
	const commit = payload.after ?? "";
	if (payload.deleted === true || !/^[0-9a-f]{40}$/.test(commit) || /^0+$/.test(commit))
		return Response.json({ ignored: "the branch was deleted" });
	if (payload.repository?.private === true)
		return refused(422, "a private mirror cannot be read: the endpoint fetches mirrors without credentials", context);
	if (await pausedBy(env)) return refused(503, "this remote is paused", context);

	// Landing here is a merge by whoever pushed there: they must be able to
	// merge here, and what they do not maintain stays as it is.
	const list = (await project.branch(PULL_BRANCH))?.list;
	if (list === undefined) return refused(404, `no branch \`${PULL_BRANCH}\``, context);
	const pusher = (payload.pusher?.name ?? "").toLowerCase();
	const who = { login: pusher, member: await project.isMember(pusher) };
	const facts = await loadDefinitions(deps.repos, id, Object.keys(list));
	const ships = (tracker: string) =>
		definition.trackers.length > 0
			? definition.trackers.includes(tracker)
			: facts.get(tracker)?.audience.kind === "public";
	const writable = Object.keys(list).filter((tracker) => {
		const found = facts.get(tracker);
		return (
			found !== undefined &&
			ships(tracker) &&
			mayMaintain(found.maintainers, who, info.owner, tracker).ok &&
			decide({ kind: "tracker", project: id, tracker }, "read", who, { tracker: found }).ok
		);
	});
	if (!writable.includes("root"))
		return refused(403, `\`${pusher}\` pushed this, and may not merge into ${projectKey(id)}`, context);

	await project.queuePull({
		target,
		repo,
		branch: definition.branch,
		commit,
		pusher,
		writable,
		queuedAt: Date.now(),
		attempts: 0,
	});
	deps.waitUntil(advancePulls(env, deps, id));
	return Response.json({ queued: commit }, { status: 202 });
}

function parseProject(key: string | null) {
	if (key === null) return null;
	const [owner, name, ...rest] = key.split("/");
	return owner && name && rest.length === 0 ? { owner: owner.toLowerCase(), name } : null;
}

/**
 * Move the oldest pull on. True when there is more to do: `ProjectDO`'s alarm
 * calls again. Never throws: a failure is logged and retried, and past
 * `PULL_ATTEMPTS` it is dropped, for a redelivery to start again.
 */
export async function advancePulls(env: Env, deps: Deps, id: ProjectId): Promise<boolean> {
	const project = projectStub(env, id);
	const [pull] = await project.pulls();
	if (pull === undefined) return false;
	const log = { project: projectKey(id), target: pull.target, commit: pull.commit };
	let done: boolean;
	try {
		done = await advance(env, deps, id, pull);
	} catch (error) {
		const why = error instanceof Error ? error.message : String(error);
		const attempts = await project.pullFailed(pull);
		const givingUp = attempts >= PULL_ATTEMPTS;
		console.error(JSON.stringify({ webhook: "pull failed", ...log, why, attempts, givingUp }));
		if (!givingUp) return true;
		done = true;
		await revokeRepoTokens(deps, await project.releaseMerge(PULL_BRANCH, WEBHOOK_MERGER));
	}
	if (!done && Date.now() - pull.queuedAt > PULL_LIMIT_MS) {
		console.error(JSON.stringify({ webhook: "pull gave up waiting; redeliver the event", ...log }));
		await revokeRepoTokens(deps, await project.releaseMerge(PULL_BRANCH, WEBHOOK_MERGER));
		done = true;
	}
	if (done) await project.dropPull(pull);
	return (await project.pulls()).length > 0;
}

/** One step of `pull`. True when it is finished, whatever came of it. */
async function advance(env: Env, deps: Deps, id: ProjectId, pull: MirrorPull): Promise<boolean> {
	const key = projectKey(id);
	const project = projectStub(env, id);
	const begun = await project.beginPull(pull, PULL_LOCK_SECONDS);
	if (!begun.ok) {
		// Someone is merging, which is theirs to finish. Try again later.
		if (begun.reason === "locked") {
			console.log(JSON.stringify({ webhook: "waiting for a merge", project: key, by: begun.by, until: begun.until }));
			return false;
		}
		throw new HttpError(404, `no branch \`${PULL_BRANCH}\` in ${key}`);
	}
	await revokeRepoTokens(deps, begun.stale);
	if (!(await project.extendMerge(PULL_BRANCH, WEBHOOK_MERGER, PULL_LOCK_SECONDS))) {
		// The lock lapsed; the next step takes it again from where `main` is.
		await project.restartPull(pull);
		return false;
	}
	if (!deps.pull) throw new HttpError(501, "this remote has no Git endpoint to pull with");
	const trackers = [...new Set([...Object.keys(begun.base), "access"])];
	const repos = Object.fromEntries(
		trackers.map((tracker) => [
			tracker,
			{ url: deps.repos.url(repoName({ kind: "tracker", project: id, tracker })), auth: "" },
		]),
	);
	const plan: PullPlan = {
		dir: "/work",
		repos,
		list: begun.base,
		writable: pull.writable,
		target: pull.target,
		mirror: { url: `https://github.com/${pull.repo}.git`, branch: pull.branch, commit: pull.commit },
	};
	const answer = await deps.pull(key, plan, async () => {
		const handed = [];
		const minted: PullPlan["repos"] = {};
		for (const tracker of trackers) {
			const name = repoName({ kind: "tracker", project: id, tracker });
			const scope = pull.writable.includes(tracker) ? "write" : "read";
			const token = await deps.repos.mint(name, scope, PULL_CREDENTIAL_SECONDS);
			handed.push({ repo: name, artifactsId: token.id });
			minted[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${token.password}`)}` };
		}
		await project.recordMergeTokens(PULL_BRANCH, handed);
		return minted;
	});
	if (isChecking(answer)) return false;

	const log = { project: key, target: pull.target, commit: pull.commit, pusher: pull.pusher };
	if (answer.outcome === "landed") {
		const finished = await project.finishMerge(PULL_BRANCH, begun.base, answer.list, WEBHOOK_MERGER);
		if (!finished.ok) throw new HttpError(409, `the pull could not finish: ${finished.reason}`);
		await revokeRepoTokens(deps, finished.tokens);
		console.log(JSON.stringify({ webhook: "landed", ...log, commits: answer.commits, root: answer.list.root }));
		return true;
	}
	await revokeRepoTokens(deps, await project.releaseMerge(PULL_BRANCH, WEBHOOK_MERGER));
	if (answer.outcome === "refused") console.error(JSON.stringify({ webhook: "refused", ...log, why: answer.why }));
	else console.log(JSON.stringify({ webhook: "nothing to bring in", ...log }));
	return true;
}
