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

**Agents run newgit; humans get a window.** Every operation is a CLI
command, and the goal is machine-readable output (`--json`) on every one;
today `spawn`, `whoami`, `remote add|show|delete`, `members list`, `stop`,
and `start` have it, and `status`, `merge`, `checkpoint`, `clone`, `login`,
and `logout`, among others, do not ([#165] proposes one `--json`
contract). The web UI is a window for humans and never the only way to do
anything.

---

## Status (2026-10-28)

Deployed at `newgit.dev`: the Worker (`remote/`, TypeScript), its Durable
Objects, the Git endpoint's container, and Artifacts.
`.github/workflows/deploy-remote.yml` deploys it after CI passes on
`main`, or by hand from the Actions tab. newgit itself is developed on it:
this repository is `spheroman/newgit` on newgit.dev, and GitHub's
`Spheroman/newgit` is its export target (`.newgit/exports/github.toml`).

**Built, and run live:**

- GitHub login (device flow), project members, and a recorded,
  individually revocable token for every login, agent, and instance.
- Invite-only: only the GitHub logins in `ALLOWED_LOGINS` may use it.
- `newgit remote add`: a project as `<owner>/<name>`, one Artifacts repo per
  code tracker, seeded with its history; branch `main`. `newgit remote show
  [--json]` names the project and its members.
- `newgit members add|remove <login>` (the owner, from a login) and
  `members list [--json]`.
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
- The landing page at `/`: one file of one tracker, at its trunk, named by
  `LANDING_PAGE` in `wrangler.jsonc`. newgit.dev's is
  `remote/site/index.html` in `site`, a secret `project-devs` tracker, so
  the page is never committed and naming it in the config is what
  publishes it. A change goes live with `newgit tracker merge site` and a
  checkpoint, within a minute (the edge caches it), with no deploy. With
  no page to serve, `/` redirects to the GitHub repository.
- GitHub export (0.6.0): `newgit export --remote <NAME>` pushes the chosen
  trackers' `main`s, history and all, to the target defined in
  `.newgit/exports/<NAME>.toml`; `--dry-run` composes and checks without
  pushing, and `--force` replaces the target branch. When the target is
  ahead with the project's own commits (a pull request merged on GitHub),
  the export brings them in first instead of refusing. A target with
  `branches = true` also receives each instance's branch on every
  workspace push, so `git push` then `gh pr create` opens a real pull
  request on a newgit remote. Definitions:
  [definitions.md](../../crates/newgit/reference/definitions.md), *Export*.

**Not built yet** (more in *What's Next*):

- Overlap reporting between live instances.
- Previews, environment snapshots, and CI on push.
- The web UI.
- Forge features: pull requests, reviews, issues, checks, releases.
- CLI commands for things the API already does: by-hand agent tokens
  (create, list, revoke), and creating branches or changing the default one.
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
                                                 │     members, branches (as lists),
 workspace                                       │     instances, forks, checkpoints
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
- **ProjectDO** holds everything *about* history: members, branches,
  instances, which trackers each instance has forked, and checkpoints (its
  tables are in `remote/src/project.ts`). One per project, so concurrent
  writes are serialized. Binding records stay local, in each machine's
  `.newgit/`; a checkpoint records what its instance had bound.
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
> ([trackers.md](trackers.md), *On the remote*).
>
> [#201] proposes dropping projects as a structure: trackers all the way
> down, rooted at the user, with an instance rooted at whichever tracker it
> spawns at. This topology assumes projects until that is decided.

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
  read (*The Git endpoint* in [trackers.md](trackers.md#on-the-remote);
  [measured](remote-measurements.md#a-git-endpoint-in-a-container-2026-10-05)).

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
  stay readable in root's history ([trackers.md](trackers.md), *On the
  remote*).

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
(`project-devs`) is a list in ProjectDO: `newgit members add <login>`,
which only the project's owner may run, and only from their login (an
agent's or instance's token is refused). The creator is the first member
and cannot be removed.

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
| Writes | everything the user may | its own instance | everything its owner may: in one project if pinned (`project`), else in every project of the owner's |
| Creates tokens, projects, members, instances | yes | **no** | tokens/projects/members **no**; instances yes |
| Revoked by | the user | `newgit remove`, the owner, itself | the owner, itself |

A by-hand agent token is the escape hatch for an agent not tied to one
instance (an orchestrator in CI). Its `project` is optional: a token minted
without one reaches every project its owner may, so pin it unless the agent
really spans projects. Records name the agent either way (`agent` on
instances and checkpoints). Tokens are listed with `GET /v1/tokens` and
revoked with `DELETE /v1/tokens/<id>`; there is no CLI for minting,
listing, or revoking them yet (*What's Next*).

**Revocation is immediate, Git included.** A revoked newgit token is refused
on its next request, and every repo password it was handed is revoked at
Artifacts in the same step (each minted password is recorded against the
token that asked for it). Artifacts refuses a revoked password on the very
next Git request (verified live). The one exception is renewal: the replaced
token's repo passwords are left to expire (≤ 15 min), because renewal is
routine and cancelling them could break a push in flight.

### The credential helper

`spawn` configures a remote workspace's Git to ask newgit for passwords,
for the endpoint's host only (an empty `helper` entry first clears helpers
configured elsewhere, so a global keychain never caches a token):

```
credential.https://newgit.dev.helper =
credential.https://newgit.dev.helper = !newgit credential
credential.https://newgit.dev.useHttpPath = true
```

Git calls `newgit credential get`, and for the endpoint
(`<owner>/<name>.git`) the password is the newgit token itself: the
workspace's own inside a workspace, the login outside one. Nothing else in a workspace talks to
Artifacts with plain Git.

newgit's own Artifacts traffic (seeding, checkpoint pushes, merges,
`clone`, `spawn --from-checkpoint`) does not go through the helper. For
each repo it asks the Worker for a password (`POST
/v1/projects/<o>/<p>/credentials`, naming the repo and `read` or
`write`); the Worker checks the rules and mints a 15-minute one, and
newgit hands it to that one `git` command as an `http.extraHeader`, in the
environment so it never shows in the process list, with the helper
cleared so nothing caches it. `newgit credential` can still answer for an
Artifacts URL (asking for write, settling for read) if someone points Git
at it by hand; that needs `useHttpPath`, or Git omits the repo path.

Who the Worker mints a password for:

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

Export targets with `branches = true` ride the same push: the instance's
branch is exported to each of them, and on a newgit remote the target is the
workspace remote `gh` resolves, so `gh pr create` opens a pull request
there. The workspace's `refs/remotes/<target>/` holds the commit the target
really has; when the target withholds a tracker the branch touches, those
commits differ, and the push says to use `gh pr create --head <branch>`. A
push straight to the target's own remote is refused, since it would carry
trackers the target withholds. (`gh` resolves to the first `branches = true`
target, not the one an instance's work belongs to: [#143].)

> **Decision:** in remote mode, newgit's remote is the project's remote;
> GitHub is an export ([trackers.md](trackers.md), *Build order* step 6).
> Keeping GitHub primary puts trunk outside enforced audience.

---

## Spawn From A Checkpoint

```
newgit spawn review-a3 --from-checkpoint fix-login/ckpt_017 [--json]
```

The checkpoint is named `<instance>/<checkpoint>`, the id `newgit
checkpoints` shows where it was made, in the project the current directory
is linked to. It cannot be combined with `--from` or `--private`.

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

`newgit merge [instance] [--into main]` (the instance is inferred inside a
workspace) runs on the merger's machine, tracker by tracker: fast-forward
where the branch has not moved, `git merge-tree` where both sides changed a
tracker, nothing at all on a conflict ([trackers.md](trackers.md),
*Checkpoints, merges, and coherence*). `newgit merge main` merges the
project root instead, which is how trackers made after `remote add` reach
the remote. On the remote it begins under a short
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

**Known hole ([#228]):** nothing checks a *new* claim on a definition file
against that file's current holder, so a tracker handed one definition can
claim `access`'s, and with it every definition in the project, and lock the
owner out. The fix is a check in `loadDefinitions` (and the same rule in the
CLI) that lets delegation only go downward.

**Overlap reporting** *(not built)*: on each push, ProjectDO compares the
paths the instance touched with every other live instance's and reports
overlaps in `newgit status --json` and the web UI. `status` has no `--json`
yet, so that comes first. Pull requests, reviews, checks, and the rest of a
forge are the open *Forge parity with gh* milestone ([#160]–[#181]); their
pages in the browser are the *Web frontend* milestone ([#182]–[#200]).

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
`gc --prune=now`. The full model is in [trackers.md](trackers.md).

A checkpoint syncs a tracker by pushing its pin and the instance's branch
(and, for a per-user tracker, its `main`) to that tracker's repo. A push
sends only objects reachable from the refs pushed, so no other tracker's
content can ride along. That checkpoint is the only way a per-user
tracker's `main` reaches the remote today: `tracker capture --from-store`
and `tracker merge` move it locally only, and a tracker no instance binds
cannot be pushed at all ([#224]).

---

## Previews *(designed, not built)*

A human clicks **Open preview** on a checkpoint in the web UI (or an agent
would run `newgit preview fix-login/ckpt_017 --json`, the same
`<instance>/<checkpoint>` form as `spawn --from-checkpoint`; no `preview`
command exists yet) and gets a link to the running app. The spikes behind
this design are in [remote-measurements.md](remote-measurements.md).

[#229] (open, "preview = role replaced by container remote?") may replace
this design; until it is settled, Decisions 5–7 stand.

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
its delta (9 s and 35 MB, against 41 s and 995 MB for the first:
[round 2](remote-measurements.md#round-2-custom-images-docker-and-captured-builds)).
So layer them — environment snapshot, then CI's or the checkpoint's on top
— and each layer costs only what changed.

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
  `dockerd` starts
  ([round 3](remote-measurements.md#round-3-making-docker-and-supabase-work)).
  With that, Supabase runs in a preview.

### Auth

**Hostnames (decided).** Everything lives on `newgit.dev`, a domain used for
nothing else — no real app's cookies can reach agent-written preview code.

- `newgit.dev` — the web UI and the API (`/v1/…`), from one Worker.
- `<id>.newgit.dev` — one preview each. One level deep, because the free
  certificate covers `*.newgit.dev` only. Ids are random (`pv-…`) and can
  never equal a reserved name (`www`, `api`, `app`).

Previews and the UI are the *same site* to a browser, so preview code could
send requests to the UI with the user's cookies or plant a domain-wide
cookie. **Web sign-in is planned in [#185]:** GitHub's OAuth web flow
producing a recorded token like any other login, held in an `HttpOnly;
Secure; SameSite=Lax` session cookie, and a **CSRF token on every mutating
request**. `SameSite=Lax` alone does not help here, because a preview *is*
same-site, so the CSRF token is what stops a preview from acting as its
viewer. This section first proposed two more rules, which #185 does not
mention yet and which are still proposed on top of it:
**every newgit cookie is `__Host-` prefixed** (host-only; no subdomain can
set or overwrite it, which a CSRF token does not prevent), and **a
state-changing request must carry a matching `Origin`** (a cheap second
check). A preview gets its viewer through a redirect to `newgit.dev`, which
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

Served by the Worker. Every action is also a CLI command. Tracked as the
*Web frontend* milestone ([#182]–[#200]); the views below are [#190]
(project and checkpoint), [#196] (audience), and [#200] (**Open preview**).

- **Project view:** live instances and checkpoints, by owner (human or
  agent), with overlap warnings.
- **Checkpoint view:** the diff, the trackers it bound (with *whose* content
  a preview would use), **Open preview**, and "spawn locally" (the command to
  copy).
- **Audience view:** every tracker and who may read it.

---

## What's Next

Grouped, not strictly ordered; the milestones on GitHub hold the detail.
Each piece lands alone and keeps the suite green.

1. **CLI gaps** over API that already exists: `newgit token create|list|
   revoke` (`POST`/`GET /v1/tokens`, `DELETE /v1/tokens/<id>`), and `newgit
   branch create` / `branch default`. `--json` on the commands that lack it,
   `status` first, under one contract ([#165]).
2. **Known holes:** a definition claim that locks the owner out ([#228]); a
   per-user tracker's `main` reaching the remote only through a checkpoint
   ([#224]); the push outbox never garbage-collected ([#107]).
3. **Overlap reporting** in `newgit status --json`.
4. **Export follow-ups** (*Outside repositories as trackers*): mount
   imported trackers through one merge commit ([#139]), import an outside
   repository unchanged as a code tracker ([#140]), export a tracker as its
   own root ([#141]), pull a target's new commits back into its trackers
   ([#142]), and resolve `gh` to the right target ([#143]).
5. **Forge parity with `gh`** ([#160]–[#181]): latency budgets, `newgit
   api`, `pr create|list|view|diff|checkout|review|merge`, checks on
   checkpoints, an event stream, issues, labels, forks, search, an inbox,
   and releases. Alongside it, a published benchmark of the remote under
   agent-shaped load ([#148]).
6. **Previews:** the base image and environment snapshots keyed on resource
   versions; preview orchestration (resolve trackers, share-or-start,
   restore, project per-user trackers, start resources, serve through the
   Worker with auth) and `newgit preview --json`; checkpoint snapshots
   inside sandboxes; CI on push with the preview-ready snapshot step.
   Subject to [#229].
7. **The web UI** ([#182]–[#200]): sign-in ([#185]), the project,
   checkpoint, and audience views, and the forge's pages. **Open preview**
   ([#200]) waits for previews, so it works on day one.
8. **Beyond one owner:** orgs, with group owners, group audiences, and
   membership as a tracker ([#227]); and whether projects survive at all
   ([#201]).

---

## Decisions

Resolved loudly; each is reversible.

1. **The core is `spawn --from-checkpoint` plus trackers;** previews are the
   human window onto it.
2. **CLI-first:** every operation is a command, and the web UI never has an
   action the CLI lacks. `--json` on every command is the goal, not yet the
   fact ([#165]).
3. **Repo per tracker (per user for `user` trackers) + a fork per instance
   per changed tracker;** one Artifacts namespace, project-prefixed names.
4. **Spawning someone else's checkpoint uses your own per-user tracker
   content,** never theirs.
5. **One preview sandbox per distinct resolved environment,** shared when the
   resolved tracker revs match.
6. **Previews start from the best snapshot available:** the checkpoint's own,
   then CI's, then the environment snapshot for its resource versions.
7. **The Worker is the preview auth layer;** every request passes through it.
   (5–7 are open again in [#229].)
8. **Worker in TypeScript; CLI stays Rust.**
9. **ProjectDO is the source of truth** for members, branches, instances,
   and checkpoints when a remote is configured. Binding records stay local.
10. **Artifacts is the project remote** in remote mode; GitHub is an export
    (`newgit export --remote`).
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
- **Cold restores** of multi-GB snapshots, and placement we do not control:
  show progress, do not promise a warm start (*Measurements*).
- **If `/proc/sys` stops being remountable,** fall back to userspace
  networking (rootless Docker or Podman with `slirp4netns`/`pasta`), which
  never writes kernel sysctls.
- **Snapshot pricing** is unlisted while snapshots are in beta.
- **Fork latency:** the first checkpoint that changes a large tracker waits
  on its fork (*Measurements*).
- **The 1 GB repo cap** for large `db-snapshots` trackers.
- **Merges inside an instance's work are flattened** when it is split into
  trackers ([trackers.md](trackers.md), *Build order* step 5).
- **Networks that filter `newgit.dev`** reset TLS; explain it, and consider
  a fallback host ([#130]).

Known gaps, tracked: [#224] (per-user `main` reaches the remote only
through a checkpoint), [#107] (the push outbox is never garbage-collected),
[#130] (TLS resets), [#143] (`gh` resolves to the first `branches = true`
target), [#228] (a definition claim can lock the owner out).

---

## Measurements

The spikes behind this design (2026-10-04 and 05) and the first live run
are kept in full, dated, in
[remote-measurements.md](remote-measurements.md). The numbers the design
leans on:

| What | Measured | So |
|---|---|---|
| Artifacts fork (hono, 41 MB) | 5–22 s | fork only the trackers an instance changes |
| Artifacts full clone (hono) | 9–38 s, against 1.8 s from GitHub | clone from the local store |
| Merge in a Worker (isomorphic-git) | fails on full history (memory) | merges run on the merger's machine |
| Sandbox boot, fresh / from a snapshot | 0.2–0.3 s / ~0.6 s (one 13.6 s outlier) | starting is cheap; building the environment is not |
| Environment from nothing (Postgres + Next.js) | 215 s | what every snapshot saves |
| Snapshot, first / on top of a restore | 41 s for 995 MB / 9 s for 35 MB | snapshots are incremental: layer them |
| First page, captured production build | 5.7 s, against 10–20 s for a dev compile | capture builds in the snapshot |
| Supabase restored, containers stopped not removed | database 4.9 s, API 13.3 s | Docker works once `/proc/sys` is remounted |
| Multi-GB snapshot, cold / warm restore | 70–100 s / 5 s | warm it; show progress |
| Endpoint clone (6,173 commits), warm / restored from snapshot | 1.3–1.8 s / 1.75–2.3 s | a project's endpoint can sleep when idle |
| Live on newgit.dev: `spawn` / checkpoint with forks / `merge` / `spawn --from-checkpoint` | 0.8 s / 10.4 s / 6.4 s / 8.4 s | the same commit id comes back out |

[#107]: https://github.com/Spheroman/newgit/issues/107
[#130]: https://github.com/Spheroman/newgit/issues/130
[#139]: https://github.com/Spheroman/newgit/issues/139
[#140]: https://github.com/Spheroman/newgit/issues/140
[#141]: https://github.com/Spheroman/newgit/issues/141
[#142]: https://github.com/Spheroman/newgit/issues/142
[#143]: https://github.com/Spheroman/newgit/issues/143
[#148]: https://github.com/Spheroman/newgit/issues/148
[#160]: https://github.com/Spheroman/newgit/issues/160
[#165]: https://github.com/Spheroman/newgit/issues/165
[#181]: https://github.com/Spheroman/newgit/issues/181
[#182]: https://github.com/Spheroman/newgit/issues/182
[#185]: https://github.com/Spheroman/newgit/issues/185
[#190]: https://github.com/Spheroman/newgit/issues/190
[#196]: https://github.com/Spheroman/newgit/issues/196
[#200]: https://github.com/Spheroman/newgit/issues/200
[#201]: https://github.com/Spheroman/newgit/issues/201
[#224]: https://github.com/Spheroman/newgit/issues/224
[#227]: https://github.com/Spheroman/newgit/issues/227
[#228]: https://github.com/Spheroman/newgit/issues/228
[#229]: https://github.com/Spheroman/newgit/issues/229
