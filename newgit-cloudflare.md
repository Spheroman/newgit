# newgit on Cloudflare

*The newgit remote, built on Workers and Artifacts — and the plan to ship it
for the Cloudflare Git platform competition.*

---

## Purpose

v1 is local. Every branch instance, tracker lane, and checkpoint lives in one
machine's `.newgit/`, and the spec defers the thing that would let it leave:
the native remote (*No Native Remote*, *Future Remote-Backed Trackers*). This
document is that remote.

The occasion is Cloudflare's competition to "build the next Git platform on
Cloudflare using Workers and Artifacts." Their bar — "we aren't looking for
GitHub as it exists today with agents added on top. At a minimum, we want to
see multiple agents working on changes concurrently" — is newgit's thesis
stated by someone else. The competition is the deadline, not the reason; the
remote is v2 work we would build anyway.

The product promise extends v1's:

> Many agents, on many machines, work on one project at once. Each gets an
> instance it cannot damage anyone else from. Any of them can be undone,
> across source, trackers, and resources. Secrets reach exactly the people
> they name — enforced by the server, not declared in a file.

### Competition constraints

- Built on **Workers + Artifacts**. Workers Paid plan (we have it).
- Submission: a **5–10 minute video**, open-source code under a permissive
  license (the repo is already MIT/Apache), and running instructions.
- **Deadline: 2026-10-14.** Artifacts billing also starts that day.

---

## What Artifacts gives us

Facts from the docs (2026-10), because the design leans on each one:

- **Repos are cheap and programmatic.** Create, fork, import, delete from a
  Worker binding, the REST API, or Wrangler. The docs recommend one repo per
  agent, session, or task — "if you have 10,000 agents, create 10,000 repos."
- **Each repo is a plain Git smart-HTTP remote**:
  `https://<account>.artifacts.cloudflare.net/git/<namespace>/<repo>.git`.
  Clone/fetch over protocol v1/v2; push over v1. Some optional capabilities
  (`filter`, `include-tag`) are unsupported — so no partial clone from it.
- **Auth is a repo-scoped token**, `read` or `write`, with a TTL, format
  `art_v1_<40hex>?expires=<unix>`. Sent as a Bearer `http.extraHeader` or as
  the Basic-auth password. **No branch- or path-level permissions exist.**
- **The Worker binding reads, it does not write.** `info`, `log`,
  `readCommit`, `readTree`, `readBlob`, `readFile`, `fork`, token mint/revoke.
  There is no ref-update, commit, or merge API. Writes are `git push` — from a
  real Git client, or isomorphic-git inside a Worker.
- **Events.** `repo.created/forked/deleted/imported` per account;
  `pushed` (with ref, before/after, commits), `cloned`, `fetched`,
  `token.created/revoked` per repo. Delivered through Queues event
  subscriptions; can start a Workflow.
- **Around it:** Sandbox SDK (a container per sandbox, handed the repo as
  `ARTIFACTS_GIT_REMOTE`), ArtifactFS (blobless clone + FUSE, lazy hydration),
  `@cloudflare/ci` (runners with lockfile-keyed snapshot caching).
- **Limits:** 1 GB per repo, 32 MB per blob, 2,000 control-plane requests per
  10 s per namespace, 2,000 Git requests per 10 s per repo. Names: letters,
  digits, `.`, `_`, `-`.

---

## The Shape

```
 developer machine(s)                         Cloudflare
 ─────────────────────                        ─────────────────────────────────────────
 newgit CLI (Rust)  ── HTTPS + newgit token ─► Worker  (API + web UI)
   spawn / checkpoint / undo / merge             │
                                                 ├─► ProjectDO (Durable Object, SQLite)
 workspace clone                                 │     members, instances, binding records,
   git push ─► outbox ─► checkpoint ─┐           │     checkpoints, merge queue, path leases
                                     │           │
   git credential helper ── mints ───┼──────────►├─► Artifacts namespace <project>
     short-lived repo tokens         │           │     <p>.src          trunk (public tier)
                                     └─ push ───►│     <p>.i.<inst>     fork per instance
                                                 │     <p>.t.<tier>     tracker lanes per audience
                                                 │
                         pushed event ─► Queue ─►└─► Worker consumer: update binding record,
                                                       overlap check, notify, start CI/review
```

Three pieces, one job each:

- **Artifacts** holds every byte of history. It is the only content store.
- **ProjectDO** holds everything that is *about* history: who is in the
  project, which instances exist, what each is bound to, every checkpoint,
  the merge queue. One Durable Object per project serializes all of it, so
  two agents merging at once is a queue, not a race.
- **The Worker** is the only thing that mints Artifacts tokens. That makes it
  the place audience becomes enforcement.

The CLI stays the product. The remote is where its records and content go
when more than one machine is involved.

---

## Mapping v1 Onto The Remote

| v1 concept | Local today | On the remote |
|---|---|---|
| Project | the store repo + `.newgit/` | an Artifacts namespace + a ProjectDO |
| Source trunk | the store repo's branches | repo `<p>.src` |
| Branch instance | binding record + workspace clone | ProjectDO row + **fork** `<p>.i.<instance>` |
| Workspace `origin` push | outbox → real remote | outbox → the instance's fork |
| Tracker lane | `.newgit/snapshots/<tracker>/<rev>/` | `refs/newgit/lanes/<tracker>` in `<p>.t.<tier>` |
| Checkpoint | `.newgit/checkpoints/<inst>/*.json` + store refs | ProjectDO row + `refs/newgit/checkpoints/<id>` in each repo it names |
| `merge` (source) | — (v1 leaves source merges to Git) | merge queue in ProjectDO, executed server-side into `<p>.src` |
| `merge` (tracker) | moves the lane's LATEST | moves `refs/newgit/lanes/<tracker>` in the tier repo |
| Audience | declared intent | **enforced**: tokens for `<p>.t.<tier>` are minted only for members of `<tier>` |
| Resources | local lifecycle | still local. Stretch: run in a Sandbox container |

Resources do not move. They exist precisely because their state cannot make
the trip; a remote does not change that. What crosses is their *deposits*
(`pg_dump` into `db-snapshots`), which are tracker content like any other.

---

## Repo Topology

> **Decision (loud): one repo per audience tier, plus one fork per
> instance.** The architecture doc's rule is "one store, not
> repo-per-tracker," for atomicity, hunk privacy, and cheap reclassification.
> That rule still holds **locally**. The remote is different by construction:
> Artifacts tokens are repo-scoped, so a permission boundary *must* be a repo
> boundary. And the architecture doc already says the native remote stores
> "client-precomputed projection bundles," one per audience tier. A repo per
> tier is that design, with Artifacts as the dumb auth-gated server.

Concretely, for project `acme`:

- `acme.src` — trunk. Audience `everyone` (in the project). Source only.
- `acme.i.<instance>` — forked from `acme.src` at spawn. The instance's
  source history and checkpoint refs. Write tokens for it go **only** to that
  instance's owner. Deleted by `newgit remove`.
- `acme.t.<tier>` — one per distinct tracker audience: `acme.t.project-devs`,
  `acme.t.user-jack`. Holds lanes and lane checkpoint refs for every tracker
  with that audience. Created lazily on first push.

What this buys, stated plainly because these are real properties and not
simulations:

- **An agent cannot push to trunk.** Its credential only opens its fork.
  Trunk moves only through the merge queue.
- **An agent cannot read a tier it is not in.** The Worker will not mint the
  token. `jack-env` is unreadable to a teammate's machine — not hidden, absent.
- **An agent cannot damage another instance.** Different repos, different
  tokens.

What it does **not** buy (say so in the docs and the video):

- Isolation *within* a machine. Two agents under one OS user share the
  credential helper and can read each other's workspaces. Per-agent identity
  on one machine is future work (sandboxes give it for free; see Stretch).
- Hunk-level privacy. Tiers are path-granular, same as v1.

---

## Identity And Tokens

Two kinds of credential, never confused:

1. **newgit token** — who you are, to the Worker. Issued by `newgit login`.
2. **Artifacts repo token** — what one repo lets you do, for minutes.
   Minted by the Worker on demand, never stored long-term.

### `newgit login`

GitHub OAuth **device flow**: the CLI prints a code, the user approves in a
browser, the Worker exchanges it and issues a newgit token bound to the GitHub
login. Device flow needs no local callback server and is what `gh` does, so it
is the obvious choice. Identity is the GitHub login, so `audience =
"user:jack"` means GitHub user `jack` — no second user directory.

Project membership (`project-devs`) is a list in ProjectDO, edited with
`newgit members add <login>`. The project creator is its first member.

### The credential helper

Workspaces never hold a long-lived secret. Every remote URL newgit writes is
paired with:

```
credential.https://<account>.artifacts.cloudflare.net.helper = !newgit credential
credential.https://<account>.artifacts.cloudflare.net.useHttpPath = true
```

(`useHttpPath` is required: without it Git does not pass the repo path, and
the helper cannot tell which repo to mint for.)

Git calls `newgit credential get`; newgit asks the Worker for a token for
*that* repo (from the URL path), the Worker checks membership and scope, mints
a token with a short TTL (15 min), and the helper returns it as the password.
This is Git's own credential protocol — interposing, not emulating — so `git
push`, `git fetch`, IDEs, and `gh`-style tools all just work, and a revoked
membership takes effect within one TTL.

Scope rules the Worker applies:

| Repo | read | write |
|---|---|---|
| `<p>.src` | members | **nobody** (the merge executor only) |
| `<p>.i.<inst>` | members | the instance owner |
| `<p>.t.<tier>` | members of the tier | members of the tier |

---

## The Push Path

v1's outbox already exists so that a push checkpoints first and then forwards.
Remote mode changes only **where it forwards**:

1. Agent runs `git push` in its workspace.
2. The outbox's `pre-receive` checkpoints the instance (reason `push`).
3. It forwards to `<p>.i.<inst>` instead of the store's `origin`.
4. The checkpoint write-through tells ProjectDO the new head synchronously;
   ProjectDO updates the binding record and runs the overlap check (below)
   before the push returns.

The outbox, not an Artifacts event, is the primary signal: it is already on
the path, it is synchronous, and it needs no per-repo subscription. A
per-fork `pushed` subscription (created by the Worker at spawn, through the
REST API) is the **backstop**: it catches a push that bypassed the outbox —
the credential helper answers any Git on the machine — and ProjectDO
reconciles the binding record when it sees a head it was not told about.
Measured lag is ~5.5 s, which is fine for a backstop and too slow for
the primary.

The workspace's `origin` fetch URL points at `<p>.src`, so `git pull` shows
trunk — what an agent expects `origin` to mean. When the project also has a
GitHub remote, publishing there is the export/mirror step, not the daily push.

> **Decision:** in remote mode, Artifacts is the project's remote. GitHub
> becomes a mirror `newgit export` or a scheduled job pushes to. Keeping
> GitHub as primary would put trunk outside the merge queue and outside
> enforced audience — the two things the remote exists for.

---

## Tracker Lanes Become Git (the barrier change)

Today a lane is a directory of snapshot files plus a `LATEST` pointer. That
cannot be pushed to Artifacts, and it is a small reimplementation of what Git
objects already are. Remote mode needs lanes to be Git, and it is the better
local design regardless:

- A lane rev is a **commit** whose tree is the tracker's owned files.
  Parent: the previous lane rev. Message: what captured it.
- The lane head is `refs/newgit/lanes/<tracker>` in the store.
- Content-addressed dedupe comes free (identical tree → same tree hash; we
  still skip creating a commit when the tree is unchanged).
- A resource deposit is a capture from a directory instead of the workspace.
- Checkpoints reference lane commits by hash, and pin them with
  `refs/newgit/checkpoints/<id>/<tracker>` so `git gc` cannot drop them.

Lane commits live in the **store** repo locally, on refs Git never checks
out, so they never appear in source history or `git log`. Pushing a tier
means pushing just that tier's `refs/newgit/lanes/*` and checkpoint refs to
`<p>.t.<tier>` — Git's own refspecs do the projection.

This change renames and deletes shared code (`lane.rs`, `TrackerLane`,
snapshot layout, checkpoint resolution). Per the repo convention it **lands
first, alone**, before any parallel branch starts.

---

## Checkpoints And Undo, Remote

A checkpoint is still one coherent record across source, trackers, and
resources. In remote mode:

- **Record:** a ProjectDO row (the same `CheckpointRecord` JSON) — so any
  machine can list and restore it, and the web UI can show it.
- **Source:** the checkpoint's head and dirty commits are pushed to the
  instance fork under `refs/newgit/checkpoints/<id>/source`.
- **Trackers:** each lane commit is pushed to its tier repo under
  `refs/newgit/checkpoints/<id>/<tracker>`.
- **Resources:** unchanged. `hash` and `command` states stay local; deposits
  are tracker content and travel as above. A checkpoint restored on a
  different machine re-runs `prepare` for resources whose state is local-only,
  and says so.

`newgit undo` on the same machine works exactly as today. `newgit undo` from a
*different* machine — or `newgit spawn --from-checkpoint <id>` — fetches the
refs and materializes. That second form is the "pick up where the agent left
off, anywhere" story.

Local is a cache: when a remote is configured, ProjectDO is the source of
truth for binding records and checkpoints, and the CLI writes through to it.
Offline commands still work locally and sync on the next remote call.

---

## Merge, And Conflicts Before Merge

### Merge queue

`newgit merge <instance>` enqueues in ProjectDO. The queue is processed one
entry at a time:

1. Fetch trunk and the instance head.
2. Merge (fast-forward when possible, else a merge commit; rebase optional per
   project config).
3. On conflict: refuse, record the conflicting paths on the instance, notify
   the owner. Nothing moves.
4. On success: push to `<p>.src` with the executor's write token, then promote
   each `merge_with_source = true` tracker's bound lane rev to its lane head.
   Trackers with `merge_with_source = false` stay where they are.

**Where it runs:** isomorphic-git in the Worker, fetching shallow to the
merge base (Day 0: ~4 s on a 41 MB repo; full history exceeds Worker
memory). A Sandbox container running real Git is the fallback for a base too
deep or a tree too large; it is not built until something needs it.

### Overlap detection

Agents should not find out about a conflict at merge time. Every `pushed`
event:

1. Diffs the instance's head against its base (via `readTree`) to get the
   paths it touches.
2. Intersects with every other live instance's touched paths, stored in
   ProjectDO.
3. On overlap, records it on both instances and notifies both: in the web UI,
   in `newgit status`, and as a line the next `git push` / `git fetch` prints
   via the credential helper's channel or the outbox hook.

This is the "rethink merge conflicts" answer: conflicts are a live,
continuously recomputed property of the set of instances, visible while they
are still cheap to avoid.

---

## Web UI

Served by the same Worker. User-facing, so it is built by a taste ≥ 7 model
(see CLAUDE.md).

- **Project view:** live instances, their owners, heads, status, overlap
  warnings, the merge queue.
- **Instance view:** checkpoint timeline (source + every tracker per
  checkpoint), per-tracker diffs read with `readTree`/`readBlob`, undo-to and
  merge buttons.
- **Tier view:** which tiers exist and who is in each — what the server
  enforces, shown plainly.

No code editor and no PR conversation threads. Review is diffs plus the
checkpoint timeline; that is what an operator of many agents needs.

---

## Stretch: Instances In Sandboxes

If the core lands early: `newgit spawn --cloud` creates a Sandbox SDK
container for the instance, mounts its fork with ArtifactFS, runs resources
inside it, and exposes services on preview URLs. Port collisions disappear by
construction, and per-agent identity becomes real because each sandbox gets
its own credential. This is the most impressive demo beat and the most
uncertain to build — it ships only if Days 1–7 are on schedule.

---

## Build Order (2026-10-04 → 2026-10-14)

Each step names the files or new crates it owns, so parallel branches do not
overlap.

### Day 0 — Spikes (Oct 4–5). Gate everything.

Throwaway, in `scratch/` or a separate repo. Each answers one question:

1. Create a namespace and repo with Wrangler; clone and push with a minted
   token. *Does the basic loop work on our account?*
2. Push `refs/newgit/lanes/x` and `refs/newgit/checkpoints/y/z`; fork with
   `defaultBranchOnly: false`. *Do custom refs push, persist, and survive a
   fork? How long does a fork take?*
3. A `git credential` helper that returns a minted token. *Does Git accept it
   against the Artifacts host?*
4. isomorphic-git merge of two branches inside a Worker. *Fits CPU limits on
   a realistic repo?* If not: the same merge in a Sandbox container.
5. A queue consumer receiving `pushed`. *Latency from push to event?*

#### Day 0 results (2026-10-04)

Run against namespace `newgit-spike` with a tiny repo and an import of
`honojs/hono` (2,866 commits, 41 MB packed) as the realistic case.

| Question | Answer | Consequence |
|---|---|---|
| Basic loop | Works. Create via Wrangler, push ~1 s. Tokens are `art_v2_…?expires=` (docs say `v1`); a create-time token lasts ~24 h. | Parse the token by the `?expires=` split, never by prefix. |
| Custom refs | `refs/newgit/lanes/*`, `refs/newgit/checkpoints/*`, `refs/notes/*` all push and list. | Lanes-as-Git works on the remote as designed. |
| Fork copies refs? | **All refs, even with `default_branch_only: true`.** | Never put anything in `<p>.src` that a fork's owner may not read. It holds trunk only, so this holds today; keep it that way. |
| Fork speed | Tiny repo ~2.4 s. Hono **5–22 s**, high variance. | Fork **asynchronously** at spawn (or lazily on first push). Never block spawn on it. |
| Clone speed | Hono full clone **9–38 s** vs GitHub 1.8 s. `--depth 1` 1.1 s. Incremental fetch 0.7 s, push 1 s. | Spawn keeps cloning **from the local store**. Artifacts is a push target and a fetch source for deltas, never a full-clone source. A machine with no store does `--depth 1` then deepens in the background. |
| Token isolation | Fork token on `<p>.src` → 403 "Invalid or expired token". Read token push → 403 "Insufficient permissions". Force-push with write token is allowed. | Enforcement claims in *Repo Topology* hold. Write tokens permit history rewrites on that repo; acceptable, since a fork is its owner's. |
| Credential helper | Works for fetch and push. Artifacts answers with a Basic challenge. **Requires `credential.useHttpPath=true`**, otherwise Git omits the repo path. | `spawn` sets both `credential.<host>.helper` and `credential.<host>.useHttpPath`. |
| Merge in a Worker | isomorphic-git + memfs. Tiny repo: clean merge + push 1.7 s; conflict reported with per-file detail in 0.7 s. Hono full history: **fails, error 1102** (resource limits; the 128 MB memory cap). Hono at `depth: 10`: merge + push 3.9 s, conflict 1.2 s. | The executor fetches **shallow**, deep enough to reach the merge base — known exactly, because the binding record stores the instance's base rev. If the base is deeper than the budget allows, fall back to a Sandbox container running real Git (not spiked). |
| Push events | Per-repo `pushed` subscriptions work, but **only through the REST API** (`/event_subscriptions/subscriptions`, `source.type = "artifacts.repo"` + `namespace` + `repo_name`); Wrangler's CLI has no flag for the repo. Push-to-consumer latency **~5.5 s**. The account-level `repo.forked` event did **not** arrive within minutes. | Events cannot be the primary signal: one subscription per fork, a Cloudflare API token held by the Worker, and seconds of lag. See *The Push Path*. |

### Days 1–2 — Barrier (alone)

6. **Lanes as Git** (`lane.rs`, `checkpoint.rs`, `manager.rs` call sites,
   tests in a new `tests/lanes_git.rs`). All existing tests pass; the v1
   Standard still runs.
7. **Remote config shape** in `config.rs`: `[remote] url`, project id, and
   `storage = "remote"` accepted end to end (no network yet).

### Days 2–5 — Parallel

8. **Worker + ProjectDO** (new `remote/` TypeScript package): auth (device
   flow), members, instances, checkpoints, token minting, repo creation and
   fork. Tests with `wrangler dev` + Vitest.
9. **CLI remote client** (new `crates/newgit-core/src/remote.rs`, CLI
   commands): `login`, `remote add`, `credential`, `members`; spawn forks,
   checkpoint writes through, undo/spawn-from-checkpoint fetch.
10. **Push path:** outbox forwards to the instance fork; tier pushes for lanes.
11. **Events:** queue consumer, binding-record updates, overlap detection.
12. **Merge queue + executor.**

### Days 5–7

13. **Web UI.**
14. Stretch: sandboxes.

### Days 7–9

15. End-to-end runs of the demo script on two real machines (or two users on
    one). Fix what breaks.
16. Docs: update `newgit-v1-mvp.md` (*No Native Remote*, *Future Remote-Backed
    Trackers*, `storage`), README, `newgit reference`, changelog entries.
17. Review pass (Fable), with fixes.

### Days 9–10

18. Record the video. Submit by Oct 13, keeping Oct 14 as buffer.

---

## Demo Script (the video)

1. `newgit init` a real app, `newgit remote add`, `newgit login`. Show the
   tiers: `project-devs`, `user-jack`.
2. Spawn three agents' instances. Each is a fork; show it in the web UI
   appearing live. Each gets its own ports, DB, and `.env.local`.
3. Two agents edit overlapping files. Both are warned on push, before either
   merges.
4. One agent wrecks its instance — deletes files, drops the DB. `newgit undo`
   restores source, env, and DB.
5. From a second machine (logged in as a teammate), `spawn
   --from-checkpoint` picks up an agent's work — and `jack-env` is refused,
   with the server's message.
6. Merge two instances through the queue; trunk moves; `merge_with_source`
   trackers follow.
7. (If built) one instance running in a sandbox with a preview URL.

---

## Decisions

Resolved here, loudly, so branches can build against them. Each is
reversible after the competition.

1. **Repo per audience tier + fork per instance** on the remote; one store
   locally. (See *Repo Topology*.)
2. **Worker in TypeScript; CLI stays Rust.** The Artifacts binding's types are
   TypeScript-first, and workers-rs would fight them.
3. **ProjectDO is the source of truth** for binding records and checkpoints
   when a remote is configured. Local is a cache.
4. **Artifacts is the project remote** in remote mode; GitHub is a mirror.
5. **Identity is GitHub login**, via device flow.
6. **Tracker lanes are Git commits on `refs/newgit/lanes/*`**, locally and
   remotely.

## Open Questions

- Fork latency varies 5–22 s on a medium repo. Async fork at spawn hides it
  for the first push only if the fork finishes first; the outbox must wait on
  (or trigger) a fork that is still in flight.
- The account-level `repo.forked` event never arrived in the spike. Not
  needed by the design, but worth one more try before relying on any
  account-level event.
- The Worker needs a Cloudflare API token secret (Queues event-subscription
  permission) to create per-fork push subscriptions. Scope it to that alone.
- The 1 GB repo cap: large `db-snapshots` lanes may need per-tracker repos or
  rotation. Fine for the demo; note it.
- Rate limits are per namespace. One namespace per project is fine for the
  demo; sharding is a later concern.
