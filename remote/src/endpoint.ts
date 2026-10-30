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
// A snapshot is the whole container, the `newgit` binary included, and the
// views it composed. So a snapshot, or a running container, from an image
// other than the one deployed now is dropped: otherwise a deploy would never
// reach a project whose endpoint had been snapshotted.

import { DurableObject } from "cloudflare:workers";

/** What the Worker hands the container: `newgit endpoint-compose`'s input. */
export interface EndpointPlan {
	dir: string;
	branches: Record<string, Record<string, string>>;
	default_branch: string;
	/** The trackers holding definitions, at the default branch's commits. */
	definitions: Record<string, string>;
	repos: Record<string, { url: string; auth: string }>;
}

export type EndpointKind = "advertise" | "upload-pack";

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

	private async sleepDecision(): Promise<SleepDecision> {
		const storage = this.ctx.storage;
		return decideSleep({
			now: Date.now(),
			running: this.container().running,
			inFlight: this.inFlight,
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
		if (this.inFlight > 0) {
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
			inFlight: this.inFlight,
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
				container.start({ containerSnapshot: { id: snapshot }, enableInternet: true });
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
		const snapshot = await this.container().snapshotContainer({ name: "endpoint" });
		await this.ctx.storage.put({ snapshot: snapshot.id, snapshot_image: image, snapshot_at: startedAt });
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

	private async serve(request: Request): Promise<Response> {
		const kind = new URL(request.url).pathname.slice(1) as EndpointKind;
		const encoded = request.headers.get(PLAN_HEADER);
		if (!encoded) return new Response("no plan\n", { status: 400 });
		const plan = atob(encoded);
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
				headers: { "content-type": "application/x-git-upload-pack-advertisement", "cache-control": "no-cache" },
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
			headers: { "content-type": "application/x-git-upload-pack-result", "cache-control": "no-cache" },
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
