# The newgit remote

*Where attempts go when they leave the machine they were made on: a Worker
on Cloudflare, with Artifacts holding the history and Sandboxes for previews.*

---

## Purpose

v1 is local. Every branch instance, tracker, and checkpoint lives in one
machine's `.newgit/`. The remote is what lets an attempt travel.

Agents are cheap enough that the working loop is: send an agent off, ignore
it until it hands back work, then merge it or throw it away and retry with a
better prompt. So **attempts have to be cheap to start, cheap to throw away,
and fast to review** — and an attempt is more than its diff. It is source
*plus* the environment it ran in: env files, a database, installed
dependencies, running services.

> **Spawn any attempt anywhere and it comes up running.** A checkpoint made by
> an agent — in the cloud or on someone's laptop — can be spawned on your
> machine, or opened as a live preview from a link, with its database and
> *your* keys. Review several side by side, merge one, and tear the rest down
> completely.

Trackers and `spawn --from-checkpoint` are the core. The remote is how
checkpoints and tracker content travel; previews are how a human tries an
attempt without spawning it.

**Agents run newgit; humans get a window.** Every operation is a CLI command
with machine-readable output (`--json`). The web UI is a window for humans
and never the only way to do anything.

---

## Status (2026-10-21)

Deployed at `newgit.dev`: the Worker (`remote/`, TypeScript), its Durable
Objects, the Git endpoint's container, and Artifacts.
`.github/workflows/deploy-remote.yml` deploys it after CI passes on
`main`, or by hand from the Actions tab.

**Built, and run live:**

- GitHub login (device flow), project members, and a recorded,
  individually revocable token for every login, agent, and instance.
- Invite-only: only the GitHub logins in `ALLOWED_LOGINS` may use it.
- `newgit remote add`: a project as `<owner>/<name>`, one Artifacts repo per
  code tracker, seeded with its history; branch `main`.
- `spawn` (forks nothing), checkpoint and `git push` (split into trackers;
  only changed trackers are forked), `newgit merge` (every tracker on the
  branch moves together, or none does).
- The Git endpoint: `git clone https://newgit.dev/<owner>/<name>.git` with
  plain Git, each reader's view composed from the trackers they may read,
  with canonical-id notes on rebuilt commits.
- `newgit clone`, and `spawn --from-checkpoint` with the spawner's own
  per-user trackers.
- Private instances (`spawn --private`), hidden from everyone until merged.
- `newgit remote delete <owner>/<name>`: the owner deletes the project —
  every token pinned to it revoked first, then every repo under its prefix,
  then its record — so the name can be linked again from scratch.

**Not built yet** (order in *What's next*):

- GitHub export.
- Overlap reporting between live instances.
- Previews, environment snapshots, and CI on push.
- The web UI.
- CLI commands for things the API already does: by-hand agent tokens, and
  creating branches or changing the default one.
- Merge stacking (`merge --only`), and resources that depend on a role.

---

## What Cloudflare gives us

Facts from the docs (2026-10), because the design leans on each one.

### Artifacts

- **Repos are cheap and programmatic.** Create, fork, import, delete from a
  Worker binding, the REST API, or Wrangler. The docs recommend one repo per
  agent, session, or task.
- **Each repo is a plain Git smart-HTTP remote**:
  `https://<account>.artifacts.cloudflare.net/git/<namespace>/<repo>.git`.
  Clone/fetch over protocol v1/v2; push over v1. No `filter`, so no partial
  clone.
- **Auth is a repo-scoped token**, `read` or `write`, with a TTL. **No branch-
  or path-level permissions exist.**
- **The Worker binding reads, it does not write.** No ref-update, commit, or
  merge API. Writes are `git push`, from a real Git client or isomorphic-git
  in a Worker.
- **A Worker's binding is pinned to one namespace** in its Wrangler config.
- **Events:** per-repo `pushed` (via Queues event subscriptions, REST API
  only); account-level create/fork/delete.
- **Limits:** 1 GB per repo, 32 MB per blob, 2,000 requests per 10 s per
  namespace (control plane) and per repo (Git). Names: letters, digits, `.`,
  `_`, `-`.
- **Billing** started 2026-10-14.

### Sandboxes and Containers

- A **Sandbox** is a Linux container driven by a Durable Object. A Worker
  forwards HTTP and WebSocket requests to a port in it, so **the Worker sits
  in front of every preview request** — it is the auth layer by construction.
- **Container snapshots** (public beta) save the container's whole writable
  filesystem — not memory, not running processes. Immutable, up to 20 GB,
  retained 30 days from creation or last restore. **Tied to the image version
  they were taken from.** The pricing page lists no snapshot charge; cost is
  the time a container runs.
- **Previews on their own hostnames** need a domain on the account (wildcard
  DNS + a Worker route). Without one, previews are served under a path, which
  breaks apps that use absolute URLs.
- `@cloudflare/ci` runs CI steps in sandboxes and caches a snapshot of the
  install step keyed by the lockfile.

---

## The Shape

```
 anywhere newgit runs                          Cloudflare
 (laptop, cloud agent's sandbox, CI)           ─────────────────────────────────────────────
 newgit CLI ── HTTPS + newgit token ─────────► Worker  (API; later the web UI and previews)
   remote add / spawn / checkpoint /             │
   merge / clone / spawn --from-checkpoint       ├─► ProjectDO (Durable Object, SQLite)
                                                 │     members, instances, binding records,
 workspace                                       │     checkpoints, branches (as lists)
   git push ─► outbox ─► checkpoint ─┐           │
   git credential helper ── tokens ──┼──────────►├─► UserDO: each user's recorded tokens
                                     │           │
                                     └─ push ───►├─► Artifacts (one namespace, project-prefixed)
                                                 │     <o>.<p>.t.<tracker>          a tracker
                                                 │     <o>.<p>.t.<tracker>.<login>  per-user tracker
 git clone / git pull ──────────────────────────►│     <o>.<p>.i.<inst>.<tracker>   an instance's fork
   https://newgit.dev/<owner>/<name>.git         │
                                                 └─► Endpoint container (one per project):
                                                       composes each reader's view, serves
                                                       it with `git upload-pack`
```

- **Artifacts** holds every byte of history. It is the only content store.
- **ProjectDO** holds everything *about* history: members, instances, binding
  records, checkpoints, and branches. One per project, so concurrent writes
  are serialized.
- **The Worker** is the only thing that mints Artifacts tokens, and (once
  previews exist) the only way into a preview. That makes it the place
  audience becomes enforcement.

> **Decision (loud): one Artifacts namespace for all projects, repo names
> prefixed with the project.** A Worker's binding names one namespace in its
> config. Rate limits are per namespace; sharding is a later concern.

---

## Repo Topology

> **Decision (loud): one repo per tracker, and an instance forks only the
> trackers it changes.** A project is `<owner>/<name>`, like GitHub. On the
> remote, Artifacts tokens are repo-scoped, so a permission boundary *must*
> be a repo boundary, and a tracker is the unit audience is declared on
> (newgit-trackers.md, *On the remote*).

- `<owner>.<name>.t.<tracker>` — a code tracker's history: one Git branch per
  newgit branch (`main`), the entry in that branch's list. Readable by the
  tracker's audience; written only when it is seeded and by `newgit merge`.
  Also a shared secret tracker's content.
- `<owner>.<name>.t.<tracker>.<login>` — one user's per-user secret tracker.
- `<owner>.<name>.i.<instance>.<tracker>` — an instance's fork of one code
  tracker, made the first time the instance changes it; its branch is
  `refs/heads/<instance>` and each checkpoint's commit
  `refs/newgit/checkpoints/<id>`. Written only by that instance. Deleted by
  `newgit remove`. A private instance's forks (`newgit spawn --private`) are
  readable only by its owner, and its record and checkpoints are hidden
  from everyone else until its work is merged.
- **Branches** are lists — one commit per code tracker — in the project's
  Durable Object, where a merge moves one by compare-and-swap.
- **The Git endpoint**, `https://newgit.dev/<owner>/<name>.git`, is what
  `git clone`, `git pull`, and a workspace's `origin` fetch: each branch
  composed, in the project's container, from the trackers the caller may
  read (*The Git endpoint* in newgit-trackers.md; *Measurements* below).

Real properties, not simulations:

- **An agent cannot move a branch.** Merging is a person's decision; agent
  and instance tokens are refused.
- **An agent cannot read a tracker outside its audience.** The Worker will not
  mint the token, and the endpoint composes nothing it may not read.
- **An agent cannot damage another instance.** Different repos, different
  tokens.

Not provided, and the docs must say so:

- Isolation *within* a machine: agents under one OS user share the credential
  helper. Sandboxes give per-agent identity.
- Hunk-level privacy. Trackers are path-granular.
- Carving does not hide the past: a narrower code tracker's earlier versions
  stay readable in root's history (newgit-trackers.md, *On the remote*).

---

## Identity And Tokens

1. **newgit token** — who you are, to the Worker. Signed by the Worker and
   **recorded** in a Durable Object per user (`UserDO`); a token is good only
   while its record says so, so any one token can be revoked without logging
   anyone else out.
2. **Artifacts repo token** — what one repo lets you do, for minutes. Minted
   by the Worker on demand, never stored.

Identity is the GitHub login (`newgit login`, GitHub's device flow, like
`gh`), so `audience = "user:jack"` means GitHub user `jack`. Membership
(`project-devs`) is a list in ProjectDO: `newgit members add <login>`. The
creator is the first member.

**Invite-only (decided).** Who may use a remote at all is the Worker's
`ALLOWED_LOGINS` secret: GitHub logins separated by commas or whitespace, or
`*` for anyone. Unset admits no one, so a deployment that forgot it is
closed, not open to every GitHub account. It is checked at login and on
every request after, so taking a login off the list shuts out the tokens it
already holds, its agents' and instances' included, and the Git endpoint
with them. Repo passwords already minted for it still work at Artifacts
until they expire (≤ 15 min). Changing it is `wrangler secret put
ALLOWED_LOGINS`; no deploy is needed. Being on the list grants nothing in
any project: membership still decides that, and adding a member who is not
on the list gives them nothing until they are.

**Every workspace gets its own token, automatically** (decided). Nobody
creates tokens by hand in the normal flow:

- **`spawn` mints it.** Registering an instance with the remote returns a
  token pinned to that project and instance; the CLI keeps it inside the
  workspace's `.git/` (never committed, never pushed) and the credential
  helper uses it for anything run in that workspace.
- **It writes its own instance and nothing else:** its fork, its
  checkpoints, its head. Acting as its owner, it reads trunk and uses the
  owner's per-user trackers. It cannot touch another instance (even the same
  owner's), create or remove instances, mint tokens, or manage members.
- **It renews itself** while the instance exists: renewal returns a fresh
  token and revokes the old one, so a long-running agent never hits expiry
  and a copy stolen earlier dies at the next renewal.
- **`newgit remove` revokes it** with the fork.

This is what a sandbox, a cloud agent, or a handed-off workspace carries
*instead of* its owner's login. On the owner's own laptop, agents under the
same OS user can still read the login file (see *Repo Topology*); the
instance token matters wherever a workspace leaves the machine.

| | Login | Instance (automatic) | Agent (by hand) |
|---|---|---|---|
| Created by | `newgit login` | `spawn` | `POST /v1/tokens` (no CLI command yet) |
| Lifetime | 30 days | 7 days, renewed while the instance lives | 7 days by default, at most 30 |
| Writes | everything the user may | its own instance | everything its owner may, in its project |
| Creates tokens, projects, members, instances | yes | **no** | tokens/projects/members **no**; instances yes |
| Revoked by | the user | `newgit remove`, the owner, itself | the owner, itself |

A by-hand agent token is the escape hatch for an agent not tied to one
instance (an orchestrator in CI). Records name the agent either way
(`agent` on instances and checkpoints).

**Revocation is immediate, Git included.** A revoked newgit token is refused
on its next request, and every repo password it was handed is revoked at
Artifacts in the same step (each minted password is recorded against the
token that asked for it). Artifacts refuses a revoked password on the very
next Git request (verified live). The one exception is renewal: the replaced
token's repo passwords are left to expire (≤ 15 min), because renewal is
routine and cancelling them could break a push in flight.

### The credential helper

```
credential.https://newgit.dev.helper = !newgit credential
credential.https://<account>.artifacts.cloudflare.net.helper = !newgit credential
credential.https://<account>.artifacts.cloudflare.net.useHttpPath = true
```

Git calls `newgit credential get`. For the endpoint (`<owner>/<name>.git`)
the password is the newgit token itself — a workspace's own inside a
workspace. For an Artifacts repo, newgit asks the Worker for a password for
the repo in the URL path; the Worker checks the rules and mints a 15-minute
one. `useHttpPath` is required — without it Git omits the repo path.

| Repo | read | write |
|---|---|---|
| `<o>.<p>.t.<code tracker>` | its audience | nobody (seeding; `newgit merge`) |
| `<o>.<p>.t.<secret tracker>` | its audience | its audience |
| `<o>.<p>.t.<tracker>.<login>` | `<login>` | `<login>` |
| `<o>.<p>.i.<inst>.<tracker>` | the tracker's audience | the instance owner |

---

## The Push Path

A push from a workspace goes to newgit's outbox, which checkpoints the
instance. On a newgit remote the checkpoint's sync *is* the push: the
instance's work is split into its trackers, every commit it sends checked
for secret files, and each changed tracker's branch and the checkpoint's
commit pushed to the instance's fork of it — forked then, the first time.
The push succeeds exactly when that reached the remote. Nothing is forwarded
as one history; the endpoint composes views, and `newgit merge` moves
branches.

> **Decision:** in remote mode, newgit's remote is the project's remote;
> GitHub is an export (newgit-trackers.md, step 6). Keeping GitHub primary
> puts trunk outside enforced audience.

---

## Spawn From A Checkpoint

```
newgit spawn review-a3 --from-checkpoint ckpt_017 [--project acme] [--json]
```

1. **Record:** read the checkpoint from ProjectDO — the list of tracker
   commits it froze, the secret tracker revs it bound, its resource states.
2. **Source:** fetch each code tracker's commit from the author's fork of it
   (or the tracker's own repo, where the instance never forked it), compose
   them into the workspace's history, and clone the workspace from the
   store as v1 does. The commit id is the author's.
3. **Trackers:** resolve each secret tracker for *this* user (below), fetch
   the resolved revs from the tracker repos, project them into the
   workspace.
4. **Resources:** run this machine's `prepare`/`restore` from the same
   definitions. A database restores from the deposit the checkpoint recorded
   (a `db-snapshots` rev). Dependencies install, or clone copy-on-write from
   the install store when the identity matches. Ports are allocated here and
   rendered in.

The result is a running copy of the attempt, on this machine, with this
user's keys, uncommitted work included. `merge` and `remove` work on it like
any instance.

What travels and what does not:

- **Travels:** source, tracker content the viewer may read, and resource
  *deposits* (they are tracker content).
- **Rebuilt from definitions:** everything resources own locally — installs,
  processes, ports.
- **Handed over, not recreated:** `external` resources (a cloud preview the
  author created) come across as the author's handle; ownership decides what
  this instance's cleanup may touch.
- **Requires** the author to have run newgit and checkpointed to the remote.
  A cloud agent that only pushes a branch gives you source, nothing else.

### Resolving trackers for someone else's checkpoint

| Tracker audience | What the spawner gets |
|---|---|
| `user` (each person has their own) | **the spawner's own** `main` for that tracker |
| `project-devs`, `public` | the checkpoint's exact rev, if the spawner is in the audience |
| `user:<login>` (one named person) | refused unless the spawner is that person — loudly, naming the tracker |

So pulling a teammate's or an agent's attempt runs **their code with your
keys**, and you never receive theirs. Equal content is an equal tree, so
"same content" is a string comparison: if your `runtime-env` content equals
the agent's, the revs are equal.

### Trackers and env keys

No second file. The tracker owns the file (`runtime-env` owns `.env.local`,
holding keys); a resource renders instance values (ports, URLs) into that
same file with `[[render]]`; capture reverses the render so the tracker
holds canonical values. Every spawn, on any machine, renders its own values
back in. Resource exports (`DATABASE_URL`) reach commands through `newgit
run`. Trackers never export env.

---

## Merge

`newgit merge <instance>` runs on the merger's machine, tracker by tracker:
fast-forward where the branch has not moved, `git merge-tree` where both
sides changed a tracker, nothing at all on a conflict (newgit-trackers.md,
*Checkpoints, merges, and coherence*). On the remote it begins under a short
lock with the list it was based on, pushes each moved tracker's commit with
passwords minted for that merge, and finishes with a compare-and-swap of the
branch's list in the project's Durable Object — every tracker moves
together, or none does. (The Worker's Artifacts binding cannot write refs,
so the merge's Git work is the merger's; the list is the truth.)

Who may move a tracker is its definition's `maintainers` (`owner`,
`project-devs`, or `user:<login>`; it defaults to the audience). A
definition is itself a file, `.newgit/trackers/<name>.toml`, owned by
`access`, which only the project's owner maintains, so changing who may read
or merge anything is an owner's merge. Governance layers the same way: a
tracker that claims some definitions governs them, resolved on the Worker
from the branch's own `access` before any credential is minted. A merge
moves only the trackers the merger's instance binds; a dropped tracker stays
where the branch had it.

**Overlap reporting** *(not built)*: on each push, ProjectDO compares the
paths the instance touched with every other live instance's and reports
overlaps in `newgit status --json` and the web UI. No queue UI, no review
threads.

Undo stays as it is in v1, local only.

---

## Local Tracker Repos

Each tracker's history lives in a bare repo at `.newgit/local/trackers.git`,
separate from the project repo (secrets must never reach a `git push
--mirror` or the object store a local clone hardlinks into each workspace),
under `refs/trackers/<tracker>/`: `main`, `instances/<instance>`, and
`pins/<rev>` keeping what bindings and checkpoints name. Captures are real
commits with parents, authors, and messages; equal content is an equal
tree on any machine. Cleanup drops unrooted pins and runs
`gc --prune=now`. The full model is in `newgit-trackers.md`.

A checkpoint syncs a tracker by pushing its pin and the instance's branch
(and, for a per-user tracker, its `main`) to that tracker's repo. A push
sends only objects reachable from the refs pushed, so no other tracker's
content can ride along.

---

## Previews *(designed, not built)*

A human clicks **Open preview** on a checkpoint in the web UI (or an agent
runs `newgit preview ckpt_017 --json`) and gets a link to the running app.
The spikes behind this design are in *Measurements*.

### One sandbox per distinct environment

1. Resolve the viewer's trackers for the checkpoint (same rules as spawn).
2. If a running preview of that checkpoint has **exactly the same resolved
   tracker revs**, open it. Otherwise start a new sandbox for this viewer.

Most attempts are looked at by one person, so most previews are one sandbox.
A shared preview shares its database; the UI says how many people are on it.
A preview's environment identity is simply the set of tracker commits it
composes.

### Where a preview starts from

| Checkpoint taken... | Preview starts from |
|---|---|
| in a Cloudflare sandbox (cloud agent) | **that checkpoint's container snapshot** |
| on a laptop, then pushed | **the CI run's preview-ready snapshot** of that push |
| anywhere, with no usable snapshot | the **environment snapshot** for its resource versions, then the checkpoint applied |

Then: restore the snapshot, project the viewer's per-user trackers, re-render
ports, start the resources' long-running actions (processes are never in a
snapshot), and serve.

**Checkpoint snapshots.** When newgit checkpoints inside a sandbox:
run resource checkpoint hooks (the database becomes consistent — a dump into
its tracker), pause the resources, **remove the files of every tracker
narrower than the project** (per-user and `user:<login>` trackers), snapshot,
record the snapshot id on the checkpoint, put the files back, resume.
Snapshotting takes tens of seconds, so it runs in the background after the
checkpoint record is written, and the id is attached when it lands.

**CI snapshots.** CI on Cloudflare runs on a push, in a sandbox on newgit's
base image: restore the environment snapshot → fetch the pushed checkpoint
with **project-audience trackers only** (CI is not a person) → `prepare`
resources → **snapshot (preview-ready)** → tests, lint, review agent. Tests
run after the snapshot so they cannot dirty it, and CI-only secrets are
present only after it. CI on GitHub Actions cannot produce these (a snapshot
restores only onto its own image); those projects fall back to the
environment snapshot.

**Snapshots are incremental:** one taken from a restored sandbox stores only
its delta. So layer them — environment snapshot, then CI's or the
checkpoint's on top — and each layer costs only what changed.

**Builds are captured, not rebuilt on preview.** A production build is a
path-dependent resource keyed on source rev, lockfile, and build-time env; CI
builds it before its snapshot step, and a preview serves it directly (5.7 s to
first page in the spike, against 10–20 s for a dev compile).

**Environment snapshots.** Keyed on `hash(every resource's definition_rev +
every identity hash)`. Built in a sandbox: install the host tools the
templates declare, run `prepare` for path-dependent resources, snapshot. A
source-only change reuses it; editing a resource or bumping a lockfile
rebuilds it. Also the fallback whenever a base-image update orphans older
snapshots.

### Honest limits

- **Removing tracker files is not removing secrets.** A file *derived* from a
  secret — a build cache with an inlined public env var, a log that printed a
  key, rows in a database — survives. newgit removes what trackers own. Say
  so in the docs; most frameworks inline only explicitly-public variables.
- **A running app has one environment.** "Preview as me" means a sandbox
  started for me. The UI labels a preview with whose environment it runs.
- **Docker-based resources** need `/proc/sys` remounted read-write before
  `dockerd` starts (*Measurements*, round 3). With that, Supabase runs in a
  preview.

### Auth

**Hostnames (decided).** Everything lives on `newgit.dev`, a domain used for
nothing else — no real app's cookies can reach agent-written preview code.

- `newgit.dev` — the web UI and the API (`/v1/…`), from one Worker.
- `<id>.newgit.dev` — one preview each. One level deep, because the free
  certificate covers `*.newgit.dev` only. Ids are random (`pv-…`) and can
  never equal a reserved name (`www`, `api`, `app`).

Previews and the UI are the *same site* to a browser, so preview code could
send requests to the UI with the user's cookies or plant a domain-wide
cookie. Two rules close that: **every newgit cookie is `__Host-` prefixed**
(host-only; no subdomain can set or overwrite it), and **every
state-changing request to the UI or API must carry a matching `Origin`**.
A preview gets its viewer through a redirect to `newgit.dev`, which
returns a short-lived signed ticket; the preview host then sets its own
`__Host-` session cookie.

**URLs the app hands to the browser** (`SUPABASE_URL`, a site URL, a base
path) must point at the preview, not `127.0.0.1`. That is a render, like
ports: a preview exports `{{preview.url}}` and per-port public URLs, and the
same `[[render]]` find/replace that writes ports writes those. Render fixes
configured URLs; it cannot fix absolute paths hard-coded in app code (`fetch
("/api")`, `<a href="/">`), which is why own-hostname previews (with a
domain) remain the robust answer and path-based previews the fallback.

Every preview request passes through the Worker before it reaches the
container, so the Worker checks the viewer's newgit session and project
membership on each request — no tunnel, no separate proxy. With a preview
domain, each preview gets its own hostname (its own origin, so one preview's
code cannot read another's cookies or storage).

---

## Web UI *(designed, not built)*

Served by the Worker. Every action is also a CLI command.

- **Project view:** live instances and checkpoints, by owner (human or
  agent), with overlap warnings.
- **Checkpoint view:** the diff, the trackers it bound (with *whose* content
  a preview would use), **Open preview**, and "spawn locally" (the command to
  copy).
- **Audience view:** every tracker and who may read it.

---

## What's Next

In order. Each lands alone and keeps the suite green.

1. **GitHub export** (newgit-trackers.md, step 6). Nobody moves their code
   off GitHub to try a tool; newgit has to sit beside it. `newgit export
   --github <repo>` pushes a composed projection of the chosen trackers'
   `main`s, so a project can live in newgit while its public face stays on
   GitHub.
2. **CLI gaps.** `newgit token create` for by-hand agent tokens, and
   `newgit branch create` / `branch default` over the API that already
   exists.
3. **Overlap reporting** in `newgit status --json`.
4. **Previews:** the base image and environment snapshots keyed on resource
   versions; preview orchestration (resolve trackers, share-or-start,
   restore, project per-user trackers, start resources, serve through the
   Worker with auth) and `newgit preview --json`; checkpoint snapshots
   inside sandboxes; CI on push with the preview-ready snapshot step.
5. **The web UI** (project, checkpoint, audience views), built after
   previews so its centerpiece, **Open preview**, works on day one.

---

## Decisions

Resolved loudly; each is reversible.

1. **The core is `spawn --from-checkpoint` plus trackers;** previews are the
   human window onto it.
2. **CLI-first:** every operation is a command with `--json`; the web UI
   never has an action the CLI lacks.
3. **Repo per tracker (per user for `user` trackers) + a fork per instance
   per changed tracker;** one Artifacts namespace, project-prefixed names.
4. **Spawning someone else's checkpoint uses your own per-user tracker
   content,** never theirs.
5. **One preview sandbox per distinct resolved environment,** shared when the
   resolved tracker revs match.
6. **Previews start from the best snapshot available:** the checkpoint's own,
   then CI's, then the environment snapshot for its resource versions.
7. **The Worker is the preview auth layer;** every request passes through it.
8. **Worker in TypeScript; CLI stays Rust.**
9. **ProjectDO is the source of truth** for binding records, checkpoints, and
   branches when a remote is configured.
10. **Artifacts is the project remote** in remote mode; GitHub is an export.
11. **Identity is GitHub login**, via device flow. **Agents act as their
    owner.**
12. **Tracker history is Git** (`refs/trackers/<tracker>/*`), in a tracker repo
    separate from the store.

## Open Questions

- **Running cost.** Artifacts billing started 2026-10-14. One person's use
  fits almost entirely in what the $5 Workers Paid plan includes; the one
  cost that grows with projects is the endpoint container. It runs on the
  default `lite` instance (1/16 vCPU, 256 MiB): about $1.70 a month per
  project even if it never sleeps, and roughly $27 on `standard-1`. It
  sleeps after 10 minutes without a request (`IDLE_MS` in
  `src/endpoint.ts`), so a project nobody is fetching costs nothing; the
  price of sleeping is a cold start, which restores from a snapshot. Worth
  watching on the dashboard once other people use it.
- **Wildcard DNS for `newgit.dev`** has to be added (a proxied `*` record)
  before own-hostname previews; the Wrangler OAuth token cannot read or
  write DNS. Jack adds it, or a token scoped to DNS edit on that zone.
- **Cold restores.** The first restore of a multi-GB snapshot on a host is
  slow (70–100 s seen); repeats are fast (5 s). Running CI and actions from
  the snapshot right after it is built warms it as a side effect, but
  placement is not ours, so a later preview can still land cold. Show
  progress; do not promise a warm start.
- **If `/proc/sys` stops being remountable,** fall back to userspace
  networking (rootless Docker or Podman with `slirp4netns`/`pasta`), which
  never writes kernel sysctls.
- **Snapshot pricing** is unlisted while snapshots are in beta.
- **Fork latency** (5–22 s): the first checkpoint of an instance that
  changes a large tracker waits on its fork.
- **The 1 GB repo cap** for large `db-snapshots` trackers.
- **Merges inside an instance's work are flattened** when it is split into
  trackers (newgit-trackers.md, build order step 5).

---

## Measurements

Spikes run while designing the remote, and the first live run. Kept because
the design leans on the numbers.

### Artifacts (2026-10-04)

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

### Sandboxes (2026-10-05)

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

#### Round 2: custom images, Docker, and captured builds

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

#### Round 3: making Docker (and Supabase) work

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

### A Git endpoint in a container (2026-10-05)

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

### Live: the whole flow on newgit.dev (2026-10-05)

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
