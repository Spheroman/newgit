// The Git endpoint's container: one per project, running real `git` and the
// `newgit` binary (docs/design/trackers.md, *The Git endpoint*). The Worker decides
// who may read what and sends a plan; this composes it (`newgit
// endpoint-compose`, cached by plan) and answers Git's smart-HTTP request by
// piping it through `git upload-pack --stateless-rpc`. The container runs no
// server: `exec` with streamed stdin and stdout is the whole transport.
//
// It sleeps after `IDLE_MS` without a request: an alarm snapshots anything
// composed since the last snapshot, then stops it. It comes back from that
// snapshot, so a cold fetch costs a restore, not a re-clone
// (docs/design/remote.md, *Measurements*). The runtime's inactivity timeout
// is the backstop, in case the alarm never runs.
//
// A merge's checks (`newgit endpoint-ancestry`, `newgit endpoint-index`) run
// in the background: the first time a large history is here (t3code, 365
// MiB) fetching it takes many minutes, longer than any one request should
// wait. The first request starts the check and is answered 202; the merge's
// client asks again every few seconds, each request also keeping this object
// alive, and the one after the check ends gets its answer (`BackgroundChecks`).
//
// An instance's fork of a tracker in Git is made the same way (`newgit
// endpoint-copy`): the Worker creates an empty repo, and this copies every
// branch of the tracker's repo into it, in the background. Artifacts' own
// fork never finished on t3code; a stepped fetch and a stepped push do.
//
// A GitHub webhook's pull (`newgit endpoint-pull`, src/webhook.ts) runs the
// same way, with `ProjectDO`'s alarm doing the asking again.
//
// A snapshot is the whole container, the `newgit` binary included, and the
// views it composed. So a snapshot, or a running container, from an image
// other than the one deployed now is dropped: otherwise a deploy would never
// reach a project whose endpoint had been snapshotted.

import { DurableObject } from "cloudflare:workers";

/** What the Worker hands the container: `newgit endpoint-compose`'s input. */
export interface EndpointPlan {
	dir: string;
	/** Each branch's index as this reader may see it: `root`'s commit and the pins they may read. */
	branches: Record<string, Record<string, string>>;
	default_branch: string;
	/** Every tracker a branch lists, and `access` always: mounts are read from its content. */
	repos: Record<string, { url: string; auth: string }>;
}

export type EndpointKind = "advertise" | "upload-pack";

/** `newgit endpoint-ancestry`'s input: how each pin a merge moves goes. */
export interface AncestryPlan {
	dir: string;
	repos: Record<string, { url: string; auth: string }>;
	checks: { tracker: string; old: string; new: string }[];
}

/** `newgit endpoint-index`'s input: is `list` the index of `list.root`? */
export interface IndexPlan {
	dir: string;
	repos: Record<string, { url: string; auth: string }>;
	list: Record<string, string>;
}

/**
 * `newgit endpoint-copy`'s input: copy every branch of `from` (a tracker's
 * repo, read) into `to` (an instance's fork, write).
 */
export interface CopyPlan {
	dir: string;
	from: { url: string; auth: string };
	to: { url: string; auth: string };
}

/** Every branch the copy made, at its commit. */
export interface CopyAnswer {
	branches: Record<string, string>;
}

export type BackgroundKind = "ancestry" | "index" | "copy" | "pull";

/**
 * `newgit endpoint-pull`'s input: bring `mirror.commit` into `main`, whose
 * index is `list`. `repos` carry a write password for `writable`, a read
 * password for the rest.
 */
export interface PullPlan {
	dir: string;
	repos: Record<string, { url: string; auth: string }>;
	list: Record<string, string>;
	writable: string[];
	target: string;
	mirror: { url: string; branch: string; commit: string };
}

/** How a pull ended. A refusal is an answer: running it again refuses again. */
export type PullAnswer =
	| { outcome: "nothing" }
	| { outcome: "landed"; list: Record<string, string>; commits: number }
	| { outcome: "refused"; why: string };

export interface IndexAnswer {
	matches: boolean;
	actual: Record<string, string>;
}

/**
 * Per tracker: whether its new pin descends from its old one, and whether
 * the new one is on that tracker's own `main`. Git's answer, from the
 * tracker's own repo.
 */
export interface AncestryAnswer {
	tracker: string;
	forward: boolean;
	onMain: boolean;
}

/** A check's answer is kept this long for the merge that asked to collect it. */
export const CHECK_RESULT_MS = 5 * 60_000;

/**
 * A check running longer than this counts as failed. The whole of t3code
 * comes in one fetch in about five minutes, and in 2048-commit bands in
 * about ten; this is well past both.
 */
export const CHECK_LIMIT_MS = 45 * 60_000;

/**
 * A copy running longer than this counts as failed. A copy is a check's
 * fetch and then a push of the same history in 64 MiB steps, so it may run
 * well past one; asked again, it carries on from what got there.
 */
export const COPY_LIMIT_MS = 2 * 60 * 60_000;

/** The body of every answer to a check that is still running. */
export const CHECKING = { checking: true } as const;

/** A finished check: Git's answer as JSON, or why it failed. */
export type CheckOutcome = { ok: true; json: string } | { ok: false; why: string };

interface CheckEntry {
	startedAt: number;
	limitMs: number;
	outcome?: CheckOutcome;
	finishedAt?: number;
	run?: Promise<void>;
}

/**
 * The checks this object is running or has finished, by key. In memory only:
 * an object evicted mid-check loses it, and the next request starts the
 * check again, which fetches only what the first did not get into the store.
 * Pure of the container, so the tests can drive it.
 */
export class BackgroundChecks {
	private readonly entries = new Map<string, CheckEntry>();

	constructor(private readonly now: () => number = Date.now) {}

	/**
	 * What is known of check `key`: still running, its outcome (forgotten once
	 * collected), or nothing — never started, collected already, or expired.
	 */
	collect(key: string): "running" | CheckOutcome | undefined {
		this.prune();
		const entry = this.entries.get(key);
		if (!entry) return undefined;
		if (!entry.outcome) return "running";
		this.entries.delete(key);
		return entry.outcome;
	}

	/** Start check `key`, unless it already is; resolves when it ends. */
	start(key: string, run: () => Promise<CheckOutcome>, limitMs = CHECK_LIMIT_MS): Promise<void> {
		const known = this.entries.get(key);
		if (known?.run) return known.run;
		const entry: CheckEntry = { startedAt: this.now(), limitMs };
		this.entries.set(key, entry);
		entry.run = run()
			.catch((error: unknown): CheckOutcome => ({
				ok: false,
				why: String(error),
			}))
			.then((outcome) => {
				// A check given up on for running too long, or one replaced since,
				// does not answer for the one in its place.
				if (this.entries.get(key) !== entry || entry.outcome) return;
				entry.outcome = outcome;
				entry.finishedAt = this.now();
			});
		return entry.run;
	}

	/** How many checks are running: the idle alarm must not stop the container under them. */
	running(): number {
		this.prune();
		return [...this.entries.values()].filter((entry) => !entry.outcome).length;
	}

	private prune(): void {
		const now = this.now();
		for (const [key, entry] of this.entries) {
			if (entry.outcome) {
				if (now - (entry.finishedAt ?? now) > CHECK_RESULT_MS) this.entries.delete(key);
			} else if (now - entry.startedAt > entry.limitMs) {
				entry.outcome = {
					ok: false,
					why: `it ran longer than ${entry.limitMs / 60_000} minutes; ask again to start it over`,
				};
				entry.finishedAt = now;
			}
		}
	}
}

/**
 * The key of a check: its kind and its plan without credentials, which are
 * minted afresh for each request and would otherwise make every poll a new
 * check.
 */
export async function checkKey(
	kind: "ancestry" | "index",
	plan: { repos: Record<string, { url: string }> },
): Promise<string> {
	const repos = Object.fromEntries(Object.entries(plan.repos).map(([tracker, repo]) => [tracker, repo.url]));
	const text = JSON.stringify({ kind, plan: { ...plan, repos } });
	const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
	return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * The key of a copy: the fork it fills. One copy into a fork at a time,
 * whatever credentials each request carries.
 */
export function copyKey(plan: { to: { url: string } }): string {
	return `copy ${plan.to.url}`;
}

/** The key of a pull: what it brings in, on top of what. */
export function pullKey(plan: Pick<PullPlan, "list" | "mirror">): string {
	return `pull ${plan.mirror.url} ${plan.mirror.commit} ${plan.list.root}`;
}

/** Snapshot at most this often, after a compose changed what is on disk. */
const SNAPSHOT_EVERY_MS = 10 * 60_000;

/** How long the container runs after its last request. */
export const IDLE_MS = 10 * 60_000;

/**
 * How long the runtime keeps it running once nothing calls the Durable
 * Object. Longer than `IDLE_MS`, so the alarm normally stops it first,
 * after its snapshot; this only stops a container the alarm missed.
 */
const BACKSTOP_MS = IDLE_MS + 5 * 60_000;

/** When an idle alarm finds a fetch still streaming, it looks again this much later. */
const BUSY_RETRY_MS = 60_000;

export const PLAN_HEADER = "x-newgit-plan";

export type StartDecision = "running" | "replace" | "from-snapshot" | "from-image";

/**
 * How to get a container on `image`, the one deployed now. Pure, so the tests
 * can check it without a container. A container from another image is
 * replaced unless another response is still streaming from it (`inFlight`
 * counts this request too); the next request replaces it then.
 */
export function decideStart(state: {
	image: string;
	running: boolean;
	runningImage?: string;
	inFlight: number;
	snapshot?: string;
	snapshotImage?: string;
}): StartDecision {
	if (state.running) return state.runningImage === state.image || state.inFlight > 1 ? "running" : "replace";
	return state.snapshot && state.snapshotImage === state.image ? "from-snapshot" : "from-image";
}

export type SleepDecision = "not-running" | "busy" | "too-soon" | "snapshot-then-stop" | "stop";

/** What the idle alarm should do. Pure, so the tests can check it without a container. */
export function decideSleep(state: {
	now: number;
	running: boolean;
	inFlight: number;
	lastRequestAt: number;
	composedAt: number;
	snapshotAt: number;
}): SleepDecision {
	if (!state.running) return "not-running";
	if (state.inFlight > 0) return "busy";
	if (state.now - state.lastRequestAt < IDLE_MS) return "too-soon";
	return state.composedAt > state.snapshotAt ? "snapshot-then-stop" : "stop";
}

export class GitEndpoint extends DurableObject<Env> {
	/** Fetches whose response is still streaming. Lost on eviction, which only happens when it is 0. */
	private inFlight = 0;
	/**
	 * Whether this object has set the container's backstop. Not persisted: a
	 * restarted object (a deploy) sets it again. Only a running container
	 * takes one — setting it before the start throws, and thrown from the
	 * constructor that failed every request the object ever got.
	 */
	private backstopSet = false;
	/** The start under way, which concurrent requests share. */
	private starting?: Promise<Container>;
	/** The merge checks running here, and the answers waiting to be collected. */
	private readonly checks = new BackgroundChecks();

	private async sleepDecision(): Promise<SleepDecision> {
		const storage = this.ctx.storage;
		return decideSleep({
			now: Date.now(),
			running: this.container().running,
			// A check runs past the request that started it.
			inFlight: this.inFlight + this.checks.running(),
			lastRequestAt: (await storage.get<number>("request_at")) ?? 0,
			composedAt: (await storage.get<number>("composed_at")) ?? 0,
			snapshotAt: (await storage.get<number>("snapshot_at")) ?? 0,
		});
	}

	async alarm(): Promise<void> {
		let decision = await this.sleepDecision();
		if (decision === "snapshot-then-stop") {
			try {
				await this.takeSnapshot();
			} catch (error) {
				// Stop anyway: the older snapshot only costs the next cold
				// start a fetch of what changed since.
				console.error("endpoint snapshot before sleep failed", error);
			}
			// A request may have arrived while the snapshot ran.
			decision = await this.sleepDecision();
			if (decision === "snapshot-then-stop") decision = "stop";
		}
		if (decision === "busy") await this.ctx.storage.setAlarm(Date.now() + BUSY_RETRY_MS);
		// `not-running`, or `too-soon`: a request since this alarm was set moved it.
		if (decision !== "stop") return;
		// Checked again with nothing awaited before the stop, so a fetch that
		// started during the decision is never cut off.
		if (this.inFlight + this.checks.running() > 0) {
			await this.ctx.storage.setAlarm(Date.now() + BUSY_RETRY_MS);
			return;
		}
		await this.container().destroy("idle");
	}

	private container(): Container {
		const container = this.ctx.container;
		if (!container) throw new Error("the Git endpoint has no container binding");
		return container;
	}

	/** The container, running, with its backstop set. */
	private async ensureRunning(): Promise<Container> {
		const container = await this.startContainer();
		if (!this.backstopSet) {
			await container.setInactivityTimeout(BACKSTOP_MS);
			this.backstopSet = true;
		}
		return container;
	}

	/**
	 * Start the container — from its last snapshot when that was taken on the
	 * image deployed now. One start at a time: a request arriving while another
	 * replaces or starts the container waits for that, rather than starting it
	 * twice or using it before it is up.
	 */
	private startContainer(): Promise<Container> {
		this.starting ??= this.bringUp().finally(() => {
			this.starting = undefined;
		});
		return this.starting;
	}

	private async bringUp(): Promise<Container> {
		const container = this.container();
		const storage = this.ctx.storage;
		const image = container.images.endpoint;
		let decision = decideStart({
			image,
			running: container.running,
			runningImage: await storage.get<string>("running_image"),
			// A check running on the old image is not cut off under it either.
			inFlight: this.inFlight + this.checks.running(),
			snapshot: await storage.get<string>("snapshot"),
			snapshotImage: await storage.get<string>("snapshot_image"),
		});
		if (decision === "running") return container;
		if (decision === "replace") {
			await container.destroy("replaced by a deploy");
			decision = (await storage.get<string>("snapshot_image")) === image ? "from-snapshot" : "from-image";
		}
		this.backstopSet = false;
		const snapshot = await storage.get<string>("snapshot");
		await storage.put("running_image", image);
		if (decision === "from-snapshot" && snapshot) {
			try {
				container.start({
					containerSnapshot: { id: snapshot },
					enableInternet: true,
				});
			} catch {
				await storage.delete(["snapshot", "snapshot_image"]);
				container.start({ image, enableInternet: true });
			}
		} else {
			await storage.delete(["snapshot", "snapshot_image"]);
			container.start({ image, enableInternet: true });
		}
		const deadline = Date.now() + 60_000;
		for (;;) {
			try {
				const probe = await container.exec(["true"]);
				if ((await probe.exitCode) === 0) return container;
			} catch {
				// not up yet
			}
			if (Date.now() > deadline) {
				// A snapshot that cannot come up is dropped, so the next
				// request starts from the image instead of failing forever.
				await this.ctx.storage.delete(["snapshot", "snapshot_image"]);
				await container.destroy().catch(() => {});
				throw new Error("the Git endpoint's container did not start");
			}
			await new Promise((resolve) => setTimeout(resolve, 100));
		}
	}

	/** Snapshot the container now; `snapshot_at` is when it began, so a compose during it is not missed. */
	private async takeSnapshot(): Promise<void> {
		const startedAt = Date.now();
		const image = await this.ctx.storage.get<string>("running_image");
		const snapshot = await this.container().snapshotContainer({
			name: "endpoint",
		});
		await this.ctx.storage.put({
			snapshot: snapshot.id,
			snapshot_image: image,
			snapshot_at: startedAt,
		});
	}

	private async snapshotSoon(): Promise<void> {
		const last = (await this.ctx.storage.get<number>("snapshot_at")) ?? 0;
		if (Date.now() - last < SNAPSHOT_EVERY_MS) return;
		// Claimed up front, so concurrent fetches do not each start one.
		await this.ctx.storage.put("snapshot_at", Date.now());
		this.ctx.waitUntil(
			this.takeSnapshot().catch(async (error: unknown) => {
				console.error("endpoint snapshot failed", error);
				// Give the claim back, so the idle alarm knows there is still
				// something to snapshot.
				await this.ctx.storage.put("snapshot_at", last);
			}),
		);
	}

	async fetch(request: Request): Promise<Response> {
		this.inFlight++;
		let response: Response;
		try {
			// Before serving, so a request that fails still schedules the sleep.
			await this.ctx.storage.put("request_at", Date.now());
			await this.ctx.storage.setAlarm(Date.now() + IDLE_MS);
			response = await this.serve(request);
		} catch (error) {
			this.inFlight--;
			throw error;
		}
		if (!response.body) {
			this.inFlight--;
			return response;
		}
		// Done when the body has been read to the end or abandoned.
		let done = false;
		const finish = () => {
			if (!done) {
				done = true;
				this.inFlight--;
			}
		};
		const body = response.body.pipeThrough(new TransformStream({ flush: finish, cancel: finish }));
		return new Response(body, response);
	}

	/**
	 * A merge check or a fork's copy, in the background: started by the first request with
	 * its plan, answered 202 while it runs, and its answer handed to the
	 * first request after it ends. A `peek` carries no credentials: it only
	 * asks after a check already known, and is answered 404 when there is
	 * none, so the Worker mints passwords only for a check it starts.
	 */
	private async check(kind: BackgroundKind, plan: string, peek: boolean): Promise<Response> {
		const parsed = JSON.parse(plan);
		const key =
			kind === "copy" ? copyKey(parsed) : kind === "pull" ? pullKey(parsed) : await checkKey(kind, parsed);
		const known = this.checks.collect(key);
		if (known === "running") return Response.json(CHECKING, { status: 202 });
		if (known?.ok)
			return new Response(known.json, {
				headers: { "content-type": "application/json" },
			});
		// Only the merger asked, and Git's errors carry no credential (the
		// plan passes them as a header): say why, so a failure is fixable.
		if (known) return new Response(`newgit endpoint-${kind} failed: ${known.why.slice(-800)}\n`, { status: 502 });
		if (peek) return new Response("no such check\n", { status: 404 });
		// Held by `checks` and by `waitUntil`: the object stays up while the
		// merger's polls keep arriving, and the promise is what they collect.
		this.ctx.waitUntil(
			this.checks.start(key, () => this.runCheck(kind, plan), kind === "copy" ? COPY_LIMIT_MS : CHECK_LIMIT_MS),
		);
		return Response.json(CHECKING, { status: 202 });
	}

	private async runCheck(kind: BackgroundKind, plan: string): Promise<CheckOutcome> {
		const container = await this.ensureRunning();
		const check = await container.exec(["newgit", `endpoint-${kind}`], {
			stdin: new Response(plan).body!,
			stdout: "pipe",
			stderr: "pipe",
		});
		const checked = await check.output();
		// A check fetches every tracker it looks at into the store, which
		// for a large history (t3code, 365 MiB) takes minutes. Keep it like
		// a composed view, so the container does not sleep without saving
		// it and fetch the whole history again next time — a failed one too,
		// since a stepped fetch that stopped part way carries on from there.
		await this.ctx.storage.put("composed_at", Date.now());
		await this.snapshotSoon();
		if (checked.exitCode !== 0) {
			const why = new TextDecoder().decode(checked.stderr).trim();
			console.error(`endpoint-${kind} failed`, why);
			return { ok: false, why };
		}
		return { ok: true, json: new TextDecoder().decode(checked.stdout) };
	}

	private async serve(request: Request): Promise<Response> {
		const url = new URL(request.url);
		const kind = url.pathname.slice(1) as EndpointKind | BackgroundKind;
		const encoded = request.headers.get(PLAN_HEADER);
		if (!encoded) return new Response("no plan\n", { status: 400 });
		const plan = atob(encoded);
		if (kind === "ancestry" || kind === "index" || kind === "copy" || kind === "pull")
			return this.check(kind, plan, url.searchParams.has("peek"));
		const container = await this.ensureRunning();

		const compose = await container.exec(["newgit", "endpoint-compose"], {
			stdin: new Response(plan).body!,
			stdout: "pipe",
			stderr: "pipe",
		});
		const composed = await compose.output();
		if (composed.exitCode !== 0) {
			const why = new TextDecoder().decode(composed.stderr).trim();
			console.error("endpoint-compose failed", why);
			return new Response("newgit could not compose this view; the remote logged why\n", { status: 502 });
		}
		const view = new TextDecoder().decode(composed.stdout).trim();
		await this.ctx.storage.put("composed_at", Date.now());
		await this.snapshotSoon();

		const protocol = request.headers.get("git-protocol");
		const env: Record<string, string> = protocol ? { GIT_PROTOCOL: protocol } : {};
		if (kind === "advertise") {
			const process = await container.exec(["git", "upload-pack", "--stateless-rpc", "--advertise-refs", view], {
				env,
				stdout: "pipe",
				stderr: "ignore",
			});
			const { stdout } = await process.output();
			// Protocol v2 answers with its capability advertisement directly;
			// v0 needs the service preamble first.
			const body =
				protocol && protocol.includes("version=2")
					? new Uint8Array(stdout)
					: concat(pkt("# service=git-upload-pack\n"), new TextEncoder().encode("0000"), new Uint8Array(stdout));
			return new Response(body, {
				headers: {
					"content-type": "application/x-git-upload-pack-advertisement",
					"cache-control": "no-cache",
				},
			});
		}

		let input = request.body ?? new ReadableStream();
		if (request.headers.get("content-encoding") === "gzip") input = input.pipeThrough(new DecompressionStream("gzip"));
		const process = await container.exec(["git", "upload-pack", "--stateless-rpc", view], {
			env,
			stdin: input,
			stdout: "pipe",
			stderr: "ignore",
		});
		return new Response(process.stdout, {
			headers: {
				"content-type": "application/x-git-upload-pack-result",
				"cache-control": "no-cache",
			},
		});
	}
}

function pkt(line: string): Uint8Array {
	const bytes = new TextEncoder().encode(line);
	const length = (bytes.length + 4).toString(16).padStart(4, "0");
	return concat(new TextEncoder().encode(length), bytes);
}

function concat(...parts: Uint8Array[]): Uint8Array {
	const out = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
	let offset = 0;
	for (const part of parts) {
		out.set(part, offset);
		offset += part.length;
	}
	return out;
}
