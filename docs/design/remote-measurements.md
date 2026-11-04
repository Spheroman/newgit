# Remote measurements

The spikes run while designing the newgit remote, and the first live run of
the whole flow. [remote.md](remote.md) leans on these numbers and sums them
up in its *Measurements* section. Each run is dated and kept as it was
recorded, not maintained: rerun a spike before trusting a number that
matters.

---

## Artifacts (2026-10-04)

Run against namespace `newgit-spike` with a tiny repo and an import of
`honojs/hono` (2,866 commits, 41 MB packed).

| Question | Answer | Consequence |
|---|---|---|
| Basic loop | Works. Push ~1 s. Tokens are `art_v2_…?expires=` (docs say `v1`); a create-time token lasts ~24 h. | Parse tokens by the `?expires=` split, never by prefix. |
| Custom refs | `refs/newgit/*`, `refs/notes/*` push and list. | Tracker history as Git works on the remote. |
| Fork copies refs? | **All refs, even with `default_branch_only: true`.** | Nothing in a repo a fork's owner may not read. |
| Fork speed | Tiny ~2.4 s. Hono **5–22 s**, high variance. | Fork only what changed, and only when it changes. |
| Clone speed | Hono full clone **9–38 s** vs GitHub 1.8 s; `--depth 1` 1.1 s; fetch 0.7 s; push 1 s. | Clone from the local store; Artifacts is a push target and a delta source. |
| Token isolation | Fork token on another repo → 403; read token push → 403; write tokens may force-push. | Enforcement claims hold. |
| Credential helper | Works for fetch and push. **Requires `credential.useHttpPath=true`.** | `spawn` sets both. |
| Merge in a Worker | Tiny repo 1.7 s; hono full history **fails (1102, memory)**; hono at `depth: 10` 3.9 s; conflicts reported per file. | Merges run on the merger's machine with real Git. |
| Push events | Per-repo `pushed` subscriptions work **only via REST**; ~5.5 s lag. Account-level `repo.forked` never arrived. | Outbox is the primary signal; events are the backstop and the CI trigger. |

## Sandboxes (2026-10-05)

A spike Worker (key-gated) with one Durable Object class on the
`durable_object` scheduling policy, the managed `cloudflare/debian-trixie`
image (Node 24, x86_64, root, no `curl`/`wget`), instance `standard-2`. The
"realistic app": Postgres with `pgbench -s 20` (2 M rows, 580 MB on disk) and
a fresh `create-next-app` (457 MB of `node_modules`).

| Question | Answer | Consequence |
|---|---|---|
| Fresh sandbox boot | 0.2–0.3 s to accept a command. | Starting a sandbox is not the cost; building the environment is. |
| Building the environment from nothing | **215 s**: apt 86 s, Postgres init + data 16 s, `npm` 112 s. | This is what every snapshot saves. |
| Snapshot | **41 s for 995 MB** (Postgres stopped cleanly first). Returns `{id, size}`. | Taking one is too slow to sit inside an agent's checkpoint synchronously; do it in the background or at CI's step. |
| Restore boot | **0.56–0.6 s** in 4 of 5 runs; one outlier at **13.6 s**. | Plan for ~1 s, tolerate ~15 s, and show progress. |
| Restored state | Files intact: 2 M rows queryable 0.75 s after boot; Postgres up in 0.3 s. | Restoring a database from a snapshot is near-instant, unlike replaying a dump. |
| `/run` after restore | **Empty — it is a tmpfs, not in the snapshot.** Postgres failed until `/run/postgresql` was recreated. | Resources' `start` hooks must recreate runtime directories; templates should do it. |
| App ready | Next.js **dev** server: first page **10–20 s** (compile on 1 vCPU). | Previews should serve a production build, or use a bigger instance; the dev compile dominates. |
| Through the Worker | First request 0.69 s, warm 0.31 s, from outside via `getTcpPort().fetch`. | The Worker-as-front-door preview path works without a domain (path-based). |

End to end, a preview from a snapshot is **~1 s boot + ~0.3 s database +
the app's own start time**, against **215 s+** to build the same environment
from nothing.

### Round 2: custom images, Docker, and captured builds

Two named images built by Wrangler from local Dockerfiles: `base` (Node 24 on
Debian, git, Postgres, and newgit cross-compiled for x86_64 in a build stage)
and `dind` (Cloudflare's documented `docker:dind` setup plus the Supabase
CLI).

| Question | Answer | Consequence |
|---|---|---|
| newgit in a sandbox | **Works.** A full session — init, seed a tracker, spawn with a Postgres `command-snapshot` resource, checkpoint, wreck the database and `.env.local`, undo — in **4.4 s**. First boot of a custom image 5–9 s. | The base image is real; newgit needs no changes to run in a sandbox. |
| Docker in a sandbox | Docker runs. `--network=host` containers work; the default bridge network **fails** (`failed to disable IPv6 on container's interface`). | Fixed in round 3. |
| Supabase in a sandbox | Fails as configured: its database container dies on that bridge-network error. **Fixed in round 3.** | — |
| Snapshot of a restored sandbox | **Incremental:** 9 s and 35 MB, against 41 s and 995 MB for the first. | A checkpoint snapshot taken on top of an environment snapshot costs only its delta. Layer snapshots: environment → checkpoint. |
| Capturing the production build | `next build` 61 s once, captured in the snapshot. A preview restored from it serves its first page **5.7 s** after boot (including Postgres), against 10–20 s for a dev compile. | Capture builds in the snapshot. A build is a *path-dependent resource*: its identity is the source rev, lockfile, and build-time env; it is recomputed when those change and restored from the snapshot when they do not. |

### Round 3: making Docker (and Supabase) work

Root cause of the bridge failure: **`/proc/sys` is a separate read-only mount**
over a read-writable `/proc`, so every sysctl write fails — including the
per-interface IPv6 write Docker 26+ makes for each container. Bridges and
veth pairs can be created; only the sysctl writes were blocked. The sandbox's
root holds `CAP_SYS_ADMIN` and `CAP_NET_ADMIN`, and **`mount -o remount,rw
/proc/sys` succeeds**; after it, sysctl writes work, inside new network
namespaces too.

A Debian image with Docker's static binaries installed at runtime
(`docker-at <version>`), the Supabase CLI native (glibc):

| Step | Result |
|---|---|
| Remount `/proc/sys`, start Docker 29.8.2 (`--iptables=false`) | Docker up in 5 s; **bridge networking works** |
| `supabase start` from nothing | **All 12 services healthy in 122 s**, image pulls included |
| Snapshot (stopped, images + volumes, 50 k rows added) | 5.1 GB in 54 s |
| Restore, `supabase start` | 47 s (37 s with unused services excluded); data intact; REST API answers **through the Worker in 0.3–0.4 s** |
| Snapshot with containers **stopped, not removed**; restore, `docker start` | **+3 MB** delta. Database ready **4.9 s**, API healthy **13.3 s** after boot |
| Cold vs warm restore of a large snapshot | First restore of the ~5 GB chain on a host: **70–100 s**. A repeat: **5 s**. |

Consequences:

- **Docker resources work in sandboxes**, Supabase included. The base image
  remounts `/proc/sys` before starting `dockerd`. This depends on the
  sandbox's current capabilities; if Cloudflare tightens them, it breaks, so
  CI should exercise it.
- **Snapshot Docker resources stopped, not removed.** `docker start` on
  restore is ~10× faster than recreating containers.
- **Big snapshots need warming.** The first restore of a multi-GB snapshot on
  a host is slow; warm the environment snapshot right after it is built
  (restore it once in the background) so the first human preview does not pay.
- **Bake images into the environment snapshot.** Registries rate-limit
  anonymous pulls (seen in round 2); a snapshot that already holds the images
  pulls nothing.

**Caveat on captured builds:** a build inlines build-time public env vars
(`NEXT_PUBLIC_*`). Built with project-audience trackers (CI is not a person),
a viewer whose own public values differ sees the build's values until it
rebuilds with theirs. Server-side secrets are read at runtime and are
unaffected.

## A Git endpoint in a container (2026-10-05)

A spike Worker in front of a Durable Object-managed container (Debian, real
`git`, `newgit` cross-compiled for x86_64), `standard-1`. The Worker pipes
Git's smart-HTTP requests into `git upload-pack --stateless-rpc` through the
Containers `exec` API, stdin and stdout streamed; the container runs no
server. Stand-in for "fetch the tracker repos from Artifacts": a clone of
Express from GitHub (6,173 commits), adopted by newgit, `lib` carved out as
a code tracker, the view composed and served.

| Question | Answer |
|---|---|
| Cold start from the image | Boot 6.0 s; clone 2.6 s, adopt and carve 3.2 s, compose 3.1 s, publish 3.1 s — 18.7 s, 41 MB |
| The composed view | **Identical to Express's own `HEAD`** (`7ef9844`), carved tracker included |
| Plain `git clone` through the Worker, warm | **1.3–1.8 s** for all 6,173 commits; same ids; `git fsck` clean |
| Snapshot | 7.5 s, 38 MB |
| Cold clone: container stopped, restored from the snapshot | **1.75–2.3 s**, restore included (three rounds) |

Consequences:

- **The endpoint is viable as designed.** A stopped endpoint costs about
  half a second over a warm one, so per-project containers can sleep when
  idle and snapshot after each change.
- **Composing per request is cheap** — the whole 6k-commit history in about
  3 s, once, when the set of trackers changes; after that a fetch touches
  only new commits.
- No HTTP server in the container: `exec` with streamed stdin and stdout
  is the whole transport.

## Live: the whole flow on newgit.dev (2026-10-05)

A three-tracker project (root, `web` carved out of `apps/web`, project) plus
a per-user secret `.env.local` tracker, against the deployed Worker, its
endpoint container, and Artifacts.

| Step | Time | Result |
|---|---|---|
| `newgit remote add` | 9.4 s | a repo per code tracker, seeded; `main` created |
| `git clone https://newgit.dev/<owner>/<name>.git` (plain Git), cold container | 10.8 s | `HEAD` identical to the project's |
| The same, warm (view cached) | 2.6 s | |
| `spawn` | 0.8 s | nothing forked |
| Checkpoint: split, fork `root` and `web`, push | 10.4 s | |
| `git push` | 5.0 s | |
| `newgit merge` | 6.4 s | `root` and `web` fast-forward; the project root follows |
| Plain `git clone` after the merge | 7.1 s | `HEAD` is the agent's own commit id; no `.env.local` |
| `newgit clone` | 4.9 s | |
| `spawn --from-checkpoint` | 8.4 s | the same commit id, and the uncommitted work |

The commit an agent made in its workspace is the commit a plain `git clone`
gets after the merge, on another machine: split into trackers, forked,
merged per tracker, and composed back — with the same id.
