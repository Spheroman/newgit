// GitHub's push webhook, which brings merged pull requests into newgit.dev
// (docs/design/remote.md, *The GitHub webhook*). GitHub expects a quick
// answer and the work takes as long as the container does, so the route only
// verifies the event and queues it on the project; `ProjectDO`'s alarm then
// moves the queue on with `advancePulls`, which is also what keeps the
// endpoint awake while its pull runs: nobody else is polling it.
//
// A pull first looks, and ends there when there is nothing to land: every
// export of ours comes back as an event. Otherwise it lands as a merge does.
// The container pushes its commits to landing refs, `main`'s list moves by
// compare-and-swap, and the repos' branches follow. A merge that landed
// meanwhile sends it back to start from `main` as it is. Every step can run
// twice: a redelivery, an evicted object, or a retry finds the work done.

import { parse as parseToml } from "smol-toml";

import { type Deps, HttpError, isChecking, landAndFollow, loadDefinitions, projectStub, revokeRepoTokens } from "./index";
import { pausedBy } from "./budget";
import type { MirrorInstance, PullPlan } from "./endpoint";
import { decide, mayMaintain, type ProjectId, projectKey, repoName } from "./names";

/** Who a pull lands as, in `main`'s log. Not a login: logins have no spaces. */
export const WEBHOOK_MERGER = "the GitHub webhook";

/** The branch a mirror's main branch stands for, as an export pushes it. */
export const PULL_BRANCH = "main";

/** How often the alarm looks at a pull: a poll of the container. */
export const PULL_POLL_MS = 15_000;

/** How soon after an event the alarm first runs: GitHub has had its answer. */
export const PULL_START_MS = 1_000;

/** Past this, a pull still not landed gives up; a redelivery starts it again. */
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
	/** What the event says landed, and the subject of its last commit: which instance it was. */
	commits: string[];
	subject?: string;
	/** The trackers the mirror withholds. */
	withheld: string[];
	/** Instances whose withheld work this may hold, known once GitHub has said. */
	instances?: MirrorInstance[];
	/** When GitHub first listed no pull request for this, and was last asked. */
	lookup?: { since: number; last: number };
	queuedAt: number;
	attempts: number;
	/** A look found something to land. */
	peeked?: boolean;
	/** Read passwords handed out for looks, to cancel when the pull ends. */
	tokens?: { repo: string; artifactsId: string }[];
	/** `main`'s index when this pull began to land; unset until then. */
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

/** The slug newgit names an instance by: its branch, as `branch_slug` makes it. */
function instanceSlug(branch: string): string {
	return branch
		.toLowerCase()
		.replace(/[^a-z0-9]+/g, "-")
		.replace(/^-+|-+$/g, "");
}

/** The most commits one event asks GitHub about; past it, nobody can say. */
export const PULLS_ASKED = 20;

/**
 * How long "no pull request" is not believed. GitHub's list can lag the push
 * that told us, and believing it too soon lands a squash without the withheld
 * work of the instance it came from. Past this, it is a direct push.
 */
export const PULL_INDEX_WAIT_MS = 2 * 60_000;

/** How often GitHub is asked again during that wait: not every poll. */
export const PULL_INDEX_ASK_MS = 30_000;

/** How long GitHub gets to say which pull request a commit came from. */
const GITHUB_TIMEOUT_MS = 10_000;

/** The branch GitHub's merge commit subject names, if it is one. */
function mergedBranch(subject: string | undefined): string | undefined {
	return subject?.match(/^Merge pull request #\d+ from [^/\s]+\/(\S+)/)?.[1];
}

/**
 * The branches of the pull requests `commits` came from, as GitHub says. Any
 * pull request listed counts, marked merged or not: the push event can beat
 * that mark, and missing the pull request would land work without what it
 * holds. One from a fork does not: its branch names no instance here. Throws
 * when GitHub cannot answer.
 */
async function pullRequestBranches(
	fetcher: typeof fetch,
	repo: string,
	commits: string[],
): Promise<{ branches: string[]; unlisted: boolean }> {
	if (commits.length > PULLS_ASKED) throw new Error(`${commits.length} commits is too many to ask about`);
	const asked = await Promise.all(
		commits.map(async (commit) => {
			const response = await fetcher(`https://api.github.com/repos/${repo}/commits/${commit}/pulls`, {
				headers: { accept: "application/vnd.github+json", "user-agent": "newgit-remote" },
				signal: AbortSignal.timeout(GITHUB_TIMEOUT_MS),
			});
			if (!response.ok) throw new Error(`GitHub answered ${response.status} for ${commit}`);
			return (await response.json()) as { head?: { ref?: string; repo?: { full_name?: string } | null } }[];
		}),
	);
	return {
		branches: asked
			.flat()
			.filter((pull) => pull.head?.repo?.full_name?.toLowerCase() === repo.toLowerCase())
			.flatMap((pull) => pull.head?.ref ?? []),
		unlisted: asked.some((pulls) => pulls.length === 0),
	};
}

/**
 * The instances among `work` whose withheld work this pull's commits may
 * hold. A mirror's branch is named for its instance, and a push to `main`
 * does not say which branch it merged, so GitHub's merge subject says, or
 * else GitHub. `unlisted` says GitHub listed nothing for some commit, which
 * may be a direct push, or may be GitHub not having caught up: the caller
 * decides how long to wait. Only when GitHub cannot answer does every instance
 * count, and the refusal that follows can be redelivered.
 */
export async function implicated(
	deps: Deps,
	pull: Pick<MirrorPull, "repo" | "commit" | "commits" | "subject">,
	work: MirrorInstance[],
): Promise<{ instances: MirrorInstance[]; unlisted: boolean }> {
	if (work.length === 0) return { instances: [], unlisted: false };
	let branches: string[];
	let unlisted = false;
	const named = mergedBranch(pull.subject);
	if (named !== undefined) branches = [named];
	else {
		try {
			({ branches, unlisted } = await pullRequestBranches(deps.fetcher, pull.repo, [
				...new Set([...pull.commits, pull.commit]),
			]));
		} catch (error) {
			const why = error instanceof Error ? error.message : String(error);
			console.error(JSON.stringify({ webhook: "GitHub could not say which pull request this is", repo: pull.repo, commit: pull.commit, why }));
			return { instances: work, unlisted: false };
		}
	}
	const slugs = new Set(branches.map(instanceSlug));
	return { instances: work.filter((instance) => slugs.has(instance.name)), unlisted };
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
		head_commit?: { message?: string };
		commits?: { id?: string }[];
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
	const withheld = Object.keys(list).filter((tracker) => !ships(tracker));
	if (!writable.includes("root"))
		return refused(403, `\`${pusher}\` pushed this, and may not merge into ${projectKey(id)}`, context);

	await project.queuePull({
		target,
		repo,
		branch: definition.branch,
		commit,
		pusher,
		writable,
		commits: (payload.commits ?? []).flatMap((pushed) => pushed.id ?? []),
		...(payload.head_commit?.message === undefined ? {} : { subject: payload.head_commit.message }),
		withheld,
		queuedAt: Date.now(),
		attempts: 0,
	});
	return Response.json({ queued: commit }, { status: 202 });
}

function parseProject(key: string | null) {
	if (key === null) return null;
	const [owner, name, ...rest] = key.split("/");
	return owner && name && rest.length === 0 ? { owner: owner.toLowerCase(), name } : null;
}

/**
 * Move the oldest pull on. True when there is more to do. Only `ProjectDO`'s
 * alarm calls this, so two steps never run at once. Never throws: a failure
 * is logged and retried, and past `PULL_ATTEMPTS` it is dropped, for a
 * redelivery to start again.
 */
export async function advancePulls(env: Env, deps: Deps, id: ProjectId): Promise<boolean> {
	const project = projectStub(env, id);
	const [pull] = await project.pulls();
	if (pull === undefined) return false;
	const log = { project: projectKey(id), target: pull.target, commit: pull.commit };
	let done = false;
	try {
		done = await advance(env, deps, id, pull);
	} catch (error) {
		const why = error instanceof Error ? error.message : String(error);
		if (error instanceof HttpError && error.status === 409) {
			// `main` is not what this pull began from: the same base would
			// only fail again, so it starts from what `main` is now.
			console.log(JSON.stringify({ webhook: "starting again from `main`", ...log, why }));
			await project.restartPull(pull);
		} else {
			const attempts = await project.pullFailed(pull);
			done = attempts >= PULL_ATTEMPTS;
			console.error(JSON.stringify({ webhook: "pull failed", ...log, why, attempts, givingUp: done }));
		}
	}
	if (!done && Date.now() - pull.queuedAt > PULL_LIMIT_MS) {
		console.error(JSON.stringify({ webhook: "pull gave up; redeliver the event", ...log }));
		done = true;
	}
	if (done) {
		await revokeRepoTokens(deps, await project.dropPull(pull));
	}
	return (await project.pulls()).length > 0;
}

/** One step of `pull`. True when it is finished, whatever came of it. */
async function advance(env: Env, deps: Deps, id: ProjectId, pull: MirrorPull): Promise<boolean> {
	const key = projectKey(id);
	const project = projectStub(env, id);
	const log = { project: key, target: pull.target, commit: pull.commit, pusher: pull.pusher };
	let instances = pull.instances;
	const plan = (list: Record<string, string>): PullPlan => ({
		dir: "/work",
		repos: Object.fromEntries(
			trackersOf(list).map((tracker) => [
				tracker,
				{ url: deps.repos.url(repoName({ kind: "tracker", project: id, tracker })), auth: "" },
			]),
		),
		list,
		writable: pull.writable,
		target: pull.target,
		mirror: { url: `https://github.com/${pull.repo}.git`, branch: pull.branch, commit: pull.commit },
		instances: instances ?? [],
	});
	const mint = async (list: Record<string, string>, scope: (tracker: string) => "read" | "write") => {
		const handed = [];
		const minted: PullPlan["repos"] = {};
		for (const tracker of trackersOf(list)) {
			const name = repoName({ kind: "tracker", project: id, tracker });
			const token = await deps.repos.mint(name, scope(tracker), PULL_CREDENTIAL_SECONDS);
			handed.push({ repo: name, artifactsId: token.id });
			minted[tracker] = { url: deps.repos.url(name), auth: `Authorization: Basic ${btoa(`x:${token.password}`)}` };
		}
		return { handed, minted };
	};
	const ended = (answer: { outcome: "nothing" } | { outcome: "refused"; why: string }) => {
		if (answer.outcome === "refused") console.error(JSON.stringify({ webhook: "refused", ...log, why: answer.why }));
		else console.log(JSON.stringify({ webhook: "nothing to bring in", ...log }));
		return true;
	};
	if (!deps.pull) throw new HttpError(501, "this remote has no Git endpoint to pull with");

	// First a look: every export comes back as an event, and most end here.
	if (!pull.peeked) {
		const list = (await project.branch(PULL_BRANCH))?.list;
		if (list === undefined) throw new HttpError(404, `no branch \`${PULL_BRANCH}\` in ${key}`);
		const seen = await deps.pull(key, { ...plan(list), peek: true }, async () => {
			const { handed, minted } = await mint(list, () => "read");
			await project.notePullTokens(pull, handed);
			return minted;
		});
		if (isChecking(seen)) return false;
		if (seen.outcome === "nothing" || seen.outcome === "refused") return ended(seen);
		await project.markPeeked(pull);
	}

	// Asked only now: our own exports end above, and cost GitHub no call.
	if (instances === undefined) {
		const now = Date.now();
		if (pull.lookup !== undefined && now - pull.lookup.last < PULL_INDEX_ASK_MS) return false;
		const found = await implicated(deps, pull, await project.withheldWork(pull.withheld));
		const since = pull.lookup?.since ?? now;
		if (found.unlisted && now - since < PULL_INDEX_WAIT_MS) {
			await project.noteLookup(pull, { since, last: now });
			return false;
		}
		instances = found.instances;
		await project.setPullInstances(pull, instances);
	}

	const begun = await project.beginPull(pull);
	if (!begun.ok) throw new HttpError(404, `no branch \`${PULL_BRANCH}\` in ${key}`);
	const answer = await deps.pull(key, plan(begun.base), async () => {
		const { handed, minted } = await mint(begun.base, (tracker) =>
			pull.writable.includes(tracker) ? "write" : "read",
		);
		await project.notePullTokens(pull, handed);
		return minted;
	});
	if (isChecking(answer)) return false;

	if (answer.outcome === "landed") {
		// The commit point: `main`'s list, by compare-and-swap. A merge that
		// landed meanwhile sends this pull back to start from it.
		const landed = await landAndFollow(deps, project, id, PULL_BRANCH, begun.base, answer.list, WEBHOOK_MERGER, answer.moves ?? []);
		if (!landed.ok) throw new HttpError(409, `the pull could not land: \`${PULL_BRANCH}\` ${landed.reason}`);
		console.log(
			JSON.stringify({ webhook: "landed", ...log, commits: answer.commits, root: answer.list.root, behind: landed.behind }),
		);
		return true;
	}
	if (answer.outcome === "work") throw new HttpError(500, "a landing answered as a peek");
	return ended(answer);
}

const trackersOf = (list: Record<string, string>) => [...new Set([...Object.keys(list), "access"])];
