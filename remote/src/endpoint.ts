// The Git endpoint's container: one per project, running real `git` and the
// `newgit` binary (newgit-trackers.md, *The Git endpoint*). The Worker decides
// who may read what and sends a plan; this composes it (`newgit
// endpoint-compose`, cached by plan) and answers Git's smart-HTTP request by
// piping it through `git upload-pack --stateless-rpc`. The container runs no
// server: `exec` with streamed stdin and stdout is the whole transport.
//
// It sleeps when idle and comes back from a snapshot of its repositories, so
// a cold fetch costs a restore, not a re-clone (newgit-cloudflare.md, round 4).

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

export const PLAN_HEADER = "x-newgit-plan";

export class GitEndpoint extends DurableObject<Env> {
	private container(): Container {
		const container = this.ctx.container;
		if (!container) throw new Error("the Git endpoint has no container binding");
		return container;
	}

	/** Start the container — from its last snapshot when there is one. */
	private async ensureRunning(): Promise<Container> {
		const container = this.container();
		if (container.running) return container;
		const snapshot = await this.ctx.storage.get<string>("snapshot");
		const image = { image: container.images.endpoint, enableInternet: true } as const;
		if (snapshot) {
			try {
				container.start({ containerSnapshot: { id: snapshot }, enableInternet: true });
			} catch {
				await this.ctx.storage.delete("snapshot");
				container.start(image);
			}
		} else {
			container.start(image);
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
				await this.ctx.storage.delete("snapshot");
				await container.destroy().catch(() => {});
				throw new Error("the Git endpoint's container did not start");
			}
			await new Promise((resolve) => setTimeout(resolve, 100));
		}
	}

	private async snapshotSoon(): Promise<void> {
		const last = (await this.ctx.storage.get<number>("snapshot_at")) ?? 0;
		if (Date.now() - last < SNAPSHOT_EVERY_MS) return;
		await this.ctx.storage.put("snapshot_at", Date.now());
		this.ctx.waitUntil(
			this.container()
				.snapshotContainer({ name: "endpoint" })
				.then((snapshot) => this.ctx.storage.put("snapshot", snapshot.id))
				.catch((error: unknown) => console.error("endpoint snapshot failed", error)),
		);
	}

	async fetch(request: Request): Promise<Response> {
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
