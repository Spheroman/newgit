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

- `git push` in a linked project root: `newgit merge main`, then an export
  to each target whose trackers changed ([trackers.md](trackers.md),
  *Settled while building (2026-11-05)*).
- A GitHub webhook that lands what is merged on a mirror on `main`
  (*The GitHub webhook*).
- GitHub login (device flow), project members, and a recorded,
  individually revocable token for every login, agent, and instance.
- Open to any GitHub account (`ALLOWED_LOGINS = "*"`, since 2026-11-02),
  with guardrails: at most `PROJECTS_PER_LOGIN` projects per login,
  `BLOCKED_LOGINS` shut out, and `ADMIN_LOGINS` able to delete any project.
- A spend kill switch: an hourly cron pauses the remote for everyone but
  its admins while the last 7 days' bill is over `SPEND_LIMIT_WEEKLY_USD`.
- `newgit remote add`: a project as `<owner>/<name>`, one Artifacts repo per
  tracker in Git, seeded with its history; branch `main`. `newgit remote show
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
  `remote/site/index.html` in `site`, a `project-devs` tracker outside Git, so
  the page is never committed and naming it in the config is what
  publishes it. A change goes live with `newgit tracker merge site`, within
  a minute (the edge caches it), with no deploy. A shared tracker outside Git
  stored on the remote (`storage = "remote"`) has its `main` there as the
  repo's branch `main`, as a tracker in Git does, and `/` reads it.
  `newgit tracker merge` pushes it before moving `main` here, and fails
  whole if the push does, so merging such a tracker needs a login and the
  network. A checkpoint's sync carries only pins and instance branches
  (`refs/trackers/<tracker>/…`), so a tracker never merged serves nothing
  and `/` redirects to the GitHub repository. Nothing reads that `main`
  back into another machine yet: `tracker pull` there still takes its own.
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
- Resources that depend on a role. (Merge stacking by tracker, `merge
  --only`, was dropped on 2026-11-04: with root's history as the view's
  shape, a partial merge would be a cherry-pick with new ids. See
  [trackers.md](trackers.md), *`--only` is dropped*.)

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
                                                 │     members, branches (root's ref + index),
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

- `<owner>.<name>.t.<tracker>` — the history of a tracker in Git: one Git branch per
  newgit branch (`main`), the entry in that branch's list. Readable by the
  tracker's audience; written only when it is seeded and by `newgit merge`.
  Also the content of a shared tracker outside Git.
- `<owner>.<name>.t.<tracker>.<login>` — one user's per-user tracker outside Git.
- `<owner>.<name>.i.<instance>.<tracker>` — an instance's fork of one tracker
  in Git, made the first time the instance changes it (*Forks*); its branch is
  `refs/heads/<instance>` and each checkpoint's commit
  `refs/newgit/checkpoints/<id>`. Written only by that instance. Deleted by
  `newgit remove`. A private instance's forks (`newgit spawn --private`) are
  readable only by its owner, and its record and checkpoints are hidden
  from everyone else until its work is merged.
- **Branches** are lists — one commit per tracker in Git — in the project's
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
- Carving does not hide the past: the earlier versions of a narrower tracker in
  Git stay readable in root's history ([trackers.md](trackers.md), *On the
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

**Who may use it (decided).** Who may use a remote at all is the Worker's
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

newgit.dev runs with `*`, so three more secrets, in the same format and
changed the same way, keep an open remote affordable and cleanable:

- `BLOCKED_LOGINS`: shut out even under `*`, checked on every request, so
  it cuts off an abuser's tokens as removing them from the list would.
- `PROJECTS_PER_LOGIN`: how many projects one login may own (unset is 5).
  Counted in that login's UserDO when a project is created and released
  when it is deleted; projects made before the count existed are not in it.
- `ADMIN_LOGINS`: the operators. They are not capped, and may delete any
  project, member or not, which is the only takedown there is.

Nothing caps instances, forks, or storage within a project yet; the cap on
projects bounds the number of Git endpoint containers and repos per login,
not their size.

**The spend kill switch (decided).** Cloudflare has no hard spending cap,
only a budget alert that emails. So the remote keeps its own: an hourly cron
(`src/budget.ts`) reads the account's bill from the Billable Usage API,
sums `ContractedCost` (what is billed beyond the plan's included usage, not
the $5 plan itself) over today and the 6 days before it, and while that is
over `SPEND_LIMIT_WEEKLY_USD` (unset is $10) the remote is *paused*: every
request from anyone not in `ADMIN_LOGINS` is a 503 that says how much was
spent against what budget. A rolling window means it reopens on its own as
the expensive days age out; raising the budget reopens it at the next check.
The state lives in one `BudgetDO`, and each isolate caches it for 30 s.

- It needs `BILLING_TOKEN`, an API token with *Account → Billing → Read*;
  without one the switch is off. A failed read changes nothing.
- It lags. Cloudflare processes usage daily, so the switch trips about a day
  after the line is crossed, and storage already held keeps costing while
  paused. It bounds a runaway, it does not stop at the cent.
- The API selects rows by billing period and returns nothing for a range
  that starts after the period does, so the request reaches back 46 days
  and the window is applied to the rows.

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
| `<o>.<p>.t.<tracker in Git>` | its audience | nobody (seeding; `newgit merge`) |
| `<o>.<p>.t.<tracker outside Git>` | its audience | its audience |
| `<o>.<p>.t.<tracker>.<login>` | `<login>` | `<login>` |
| `<o>.<p>.i.<inst>.<tracker>` | the tracker's audience | the instance owner |

---

## The Push Path

A push from a workspace goes to newgit's outbox, which checkpoints the
instance. On a newgit remote the checkpoint's sync *is* the push: the
instance's work is split into its trackers, every commit it sends checked
for files a tracker outside Git owns (secrets among them), and each changed tracker's branch and the checkpoint's
commit pushed to the instance's fork of it — forked then, the first time.
The push succeeds exactly when that reached the remote. Nothing is forwarded
as one history; the endpoint composes views, and `newgit merge` moves
branches.

### Forks

> **Decision (loud): the remote copies a fork itself; Artifacts' `fork` is
> not used.** Artifacts' fork of Spheroman/t3code (365 MiB, 8,164 commits)
> never finished: it said `FORK_IN_PROGRESS` for over two and a half hours
> ([#144]). Git does finish, with the fetch that steps and retries and the
> push that steps past Artifacts' 64 MiB per-push limit, and the Git
> endpoint's container already runs Git.

`POST /v1/projects/:o/:n/instances/:instance/trackers/:tracker/fork`:

1. A **complete** fork is returned as it is (`200 {instance, tracker, repo,
   remote}`): the instance's work may be on it.
2. Otherwise the route creates the fork as an empty repo, mints a read
   password for the tracker's repo and a write password for the fork (130
   minutes: as long as a copy may run, and a little more), and asks the
   endpoint to copy. The container runs `newgit endpoint-copy`: fetch every
   branch of the tracker into its store, push each into the fork. The copy
   runs in the background, keyed by the fork, like a merge's checks; the
   route answers `202 {"forking": true}` with `Retry-After: 5` until it is
   done, and the client asks again. Then `200`, as in 1.
3. **Whether a fork is complete is recorded in the project's Durable
   Object** (the `forks` table's `complete` column), not in the fork. The
   object already lists each instance's forks, a row there is what the fork
   route, the credentials route, and `newgit remove` read, and the Worker can
   write it in the same step it answers 200; a marker ref would need Git to
   read and a push to write. A fork recorded before the copy finished is
   not handed out: the credentials route says it does not exist yet, and the
   next ask copies again into the same repo. The copy is idempotent — the
   fetch takes only what the store lacks, the push only what the fork lacks
   — so a copy that died part way is finished, not started over.
4. A failed copy answers `502` with Git's error as the container gave it;
   Artifacts refusing a step answers `502` with its code and message.
5. **A fork left stuck by Artifacts' fork**, which this route used before
   and which says `FORK_IN_PROGRESS` forever, is deleted and made empty
   again. A refused delete is answered `502` with Artifacts' error, not
   retried.

Every fork takes this one path, small trackers included. So a small fork
now costs a container wake (about a second from the endpoint's snapshot,
*Measurements*), a fetch and a push of the tracker, and at least one
5-second poll — against 5–22 s for Artifacts' fork of hono (41 MB). The
copy also keeps the tracker's history in the endpoint's store, so a merge's
checks on it afterwards fetch nothing new.

A tracker outside Git the remote has no definition for yet is left out of the
sync instead of failing it, with a warning that names it and quotes why.
That is a new tracker: the remote reads its audience from its definition on
`access`'s `main`, and the push that carries the definition is the one
being checkpointed. The remote answers 404 for it, not 403, because it is
an absence a later checkpoint can fill; a 403 (not a member, not in the
audience) still fails the sync. The record that goes up leaves the tracker
out too, so it never names a rev the remote does not have, and the first
checkpoint after the definition lands brings it. The local record keeps
it, so `undo` on this machine still restores it.

Export targets with `branches = true` ride the same push: the instance's
branch is exported to each target that ships a tracker its work touched, and
on a newgit remote the target is a workspace remote, the one `gh` resolves
when the work belongs to it alone, so `gh pr create` opens a pull request
there. Work that spans several targets goes to each, `gh` resolves none, and
the push names `gh pr create -R <repo> --head <branch>` for each. The workspace's `refs/remotes/<target>/` holds the commit the target
really has; when the target withholds a tracker the branch touches, those
commits differ, and the push says to use `gh pr create --head <branch>`. A
push straight to the target's own remote is refused, since it would carry
trackers the target withholds.

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
   commits it froze, the tracker outside Git revs it bound, its resource states.
2. **Source:** fetch the commit of each tracker in Git from the author's fork of it
   (or the tracker's own repo, where the instance never forked it), compose
   them into the workspace's history, and clone the workspace from the
   store as v1 does. The commit id is the author's.
3. **Trackers:** resolve each tracker outside Git for *this* user (below), fetch
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
workspace) runs on the merger's machine. The merge commit is made in the
merger's view, each tracker's files merged by `git merge-tree` (a conflict
moves nothing and names each file's tracker), and split like any other
commit, so a merger's signed merge comes back as itself
([trackers.md](trackers.md), *Checkpoints, merges, and pull*). `newgit merge
main` merges the project root instead, which is how trackers made after
`remote add` reach the remote. On the remote it begins under a short lock
with the index it was based on and the index it will make. The Worker has
the Git endpoint's container (which runs Git) say how each child's pin moves
along that child's history: forward along its `main` needs only root's
maintainers; back, sideways, or off `main` needs the child's, who get a
write password for it. A merge into `main` moves that child's `main` to
catch up; a merge into another branch keeps the child's commit on the
child's branch of that name and moves no child's `main`. Root always needs
its own. Before the new
index is written, the container checks root's pushed commit pins exactly
that, and the merge may finish only with the index it began with. It pushes each moved tracker's commit with
passwords minted for that merge, moves root's ref by one compare-and-swap,
catches each child's `main` up with what root now pins (on a merge into
`main`), and finishes by
writing the branch's list in the project's Durable Object as an index. (The
Worker's Artifacts binding cannot write refs, so the merge's Git work is the
merger's; root's ref is the truth and the list says which repos to fetch.)

The begin's ancestry check (`newgit endpoint-ancestry`) fetches every tracker
it looks at, whole, into the container's store, and the first time a large
history is there that takes many minutes (Spheroman/t3code, 365 MiB: about
five minutes in one fetch, over forty when Artifacts drops it and stepping
takes over). The finish's index check (`newgit endpoint-index`) needs root's
history and one commit's pins (*Fetching what a view needs*). Both run in the
background.
The endpoint's Durable Object keys each check by its kind and its plan
without credentials. The first request starts it and every request until it
ends is answered `202 {"checking": true}` with `Retry-After: 10`; the first
one after gets Git's answer, or the 502 with Git's error, and the answer is
forgotten. An answer nobody collects is dropped after five minutes, and a
check running past 45 minutes counts as failed. The Worker asks with a
credential-free peek first and mints read passwords (for the check's
length, not the usual fifteen minutes) only for a check it starts, so
polling mints nothing. Those passwords go only to the container, never to
the merger, and are recorded against the merger's token like any other, so
revoking it revokes them. The client asks again every ten seconds while it gets
202, says so once a minute, and gives up after an hour; every request is
short. The finish extends the merge's lock on every request, so the lock
holds while the client keeps asking. The checks are held in the object's
memory and in `waitUntil`; the polls are what keep the object alive. An
object evicted mid-check forgets it, and the next request starts it again,
against a store that keeps what the first fetched.

#### Fetching what a view needs

A view needs root's history, which is its shape, and a child's files at each
commit that history pins, never the child's history. Each deploy starts the
container from a fresh image, and every merge to `main` deploys, so a view
waiting for whole histories cost the first request after every deploy
minutes. So `endpoint-compose` fetches what it needs and no more, and the
container keeps three repositories under `/work`:

- **`trackers.git`** holds whole histories: root's, and any tracker a check,
  a copy, or a pull fetched. Every question about ancestry is asked here.
- **`snapshots.git`** is shallow on purpose: pinned commits, each fetched
  `--depth=1` by id into `refs/snapshots/<tracker>/<commit>`. It never goes
  through `fetch_refs`, which deepens any store it finds shallow. Root is
  never here: its `shallow` file would end root's history at any commit root
  shares with a child.
- **`compose.git`** reads both through alternates and holds what composing
  writes. It never fetches: Git takes an alternate's refs for complete
  history, so a store that fetches must not have a shallow alternate. That is
  why `trackers.git` has none, and why a wrong "not forward" cannot come of
  this: a merge's check never meets a shallow commit.

The ref is the proof a commit is that tracker's. A shallow fetch asks the
remote even for a commit the store already holds, and the remote refuses an
id it lacks, so one tracker's commit cannot pass as another's. A refusal does
not prove the repo lacks a commit, though: a remote may hold it and only serve
what a branch names. So a pin refused by id is settled by fetching that
tracker whole into `trackers.git`, and left out only if no branch reaches it
there either. A pin a repo really lacks therefore costs the first request on
a fresh container that tracker's whole history, once. Any other failure fails
the request rather than cache a view missing a child. One tracker's pins come
in one fetch, halved only when the remote dropped or refused it, so what they
share comes once. Only a branch whose view is not composed yet walks root's
history for pins.

What each costs on a fresh container:

| | Waits | Fetches |
|---|---|---|
| `git fetch`, first | that request | root's whole history, then one shallow fetch per tracker, and a whole tracker whose pin was refused |
| `git fetch`, next, branch unchanged | nothing | nothing |
| index check | background | root's new commits, and the one commit's pins |
| ancestry check, and a copy | background | that tracker's whole history, as it always did: 5 minutes for t3code, over 40 if stepped |

A cold `git fetch` is bounded by root's size plus its pins. For a project whose
large history is a mount, that is about 32 seconds and 113 MB for one pin
(measured from a laptop against the real t3code repo) and, unmeasured, more
for several. Where root itself is the large history, it is what it was:
that request waits for all of it. A request with no response started is cut
off at five minutes (`Network connection lost`, measured twice at 5:07), and a
cold compose of newgit with t3code took longer than that. So `info/refs` sends
its headers at once and its body when the view is ready, and a failure is an
`ERR` line, which Git prints as a `remote error`. The compose carries on if the
client leaves, and the plan's read passwords last an hour.

The client's `newgit clone` and tracker merges still fetch whole histories: a
real merge in a child needs them.

A fetch the remote drops (Artifacts fails some whole fetches with an HTTP
500, `expected 'packfile'`) is tried whole twice more, a few seconds apart,
before newgit fetches in bands: the tips at `--depth=2048`, then
`--deepen=2048` (growing to 8192) until nothing is shallow, halving a band
the remote still drops. Each band costs the remote about a minute however
small it is, so the bands are wide.

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

## The GitHub webhook

A pull request merged on a mirror lands in `main` on newgit.dev by itself.
GitHub pushes an event to `POST
/v1/webhooks/github?project=<owner>/<name>&target=<target>`, and the rest is
`newgit pull --remote <target>` for a target without `root`, run on the
remote ([trackers.md](trackers.md), *Git and GitHub*).

**What is checked, in order.** `X-Hub-Signature-256` is HMAC-SHA256 of the
raw body under `GITHUB_WEBHOOK_SECRET`, compared in constant time; unsigned
or mis-signed is 401, and with no secret configured everything is 503.
`ping` is 200, and any event but `push` is 200 and ignored. The URL names
the project and the target; root's `main` must have that
`.newgit/exports/<target>.toml`, and its `url` must be the repo the event
came from, or it is 403, so one repo's event cannot move a project that does
not say it mirrors there. A rooted target, another branch, and a deleted
branch are 200 and ignored. Whoever pushed (GitHub's `pusher.name`, which is
their newgit login) must be a member who may merge root; what they do not
maintain, and what the target withholds, gets no write password and may not
change. A private mirror is 422: the endpoint fetches mirrors without
credentials.

**What happens.** The event is queued on the project and answered 202.
Only `ProjectDO`'s alarm moves the queue on, a second later and then every
15 seconds, which is also what keeps the endpoint awake: unlike a merge
check, nobody else is asking. First `newgit endpoint-pull` looks, with read
passwords and no lock: it fetches the mirror's branch, confirms the event's
commit is on it, composes `main` as the target sees it, and walks what the
mirror has beyond that. If that is nothing, the event was our own export or
a redelivery, and it ends there, so every export coming back costs `newgit
merge` no wait, however cold the container. Otherwise the pull takes `main`'s
merge lock as `the GitHub webhook`, waiting while anyone else holds it, and
extends the lock as it runs, and the container does the same again and
lands it. The commits come in with the withheld trackers' files put back
and split, so their pins carry over; if `main` moved meanwhile, a merge made
in the view, dated by the mirror's commit so a retry makes the same one.
Root's ref moves by compare-and-swap, each child's `main` catches up, and
the Worker moves the index, ending the lock and cancelling its passwords.
Every commit comes out the same on a second run, so a run that stopped after
root moved finishes with what it pushed.

**The same refusals as a pull.** A file a tracker outside Git owns, a commit
touching a withheld tracker's mount, a conflict with `main`, a tracker the
pusher does not maintain: refused, logged, nothing moved, lock released.

**Work the mirror never saw.** A branch exported for a pull request leaves
out what the target withholds, so an instance's edit under `apps/t3code` is
on its fork and not on the mirror. Landing the merged pull request would keep
the rest of that commit and lose the edit without a word, so it is refused,
saying to run `newgit merge <instance>` and redeliver the event. A mirror's
branch is named for its instance, but a push to `main` does not say which
branch it merged. Once a look has found work to land, and only if some
instance holds withheld work at all, the subject of GitHub's merge commit
says; a squash, which is how most pull requests merge, has no such subject,
so `GET /repos/{owner}/{repo}/commits/{sha}/pulls` does, for each commit the
event landed, and each pull request's `head.ref` names an instance. Only
those instances are checked, so an unrelated squash lands while another
instance holds such work. A pull request listed there counts whether or not
it is marked merged yet, because the event can beat the mark. One from a fork
does not. GitHub's list can lag the push that told us, and believing an empty
one too soon would land a squash without the withheld work of the instance it
came from. So while some instance holds such work, a commit with no pull
request listed waits: the pull stays queued, holding no lock, and GitHub is
asked again every 30 seconds for two minutes. Only then does it count as a
direct push, which has no instance, and land. If GitHub cannot answer (a rate limit, a 5xx, more than 20 commits),
that is logged and every instance with such work counts, which is the one
case that does; the refusal can be redelivered. Those calls are
unauthenticated, 60 an hour across whatever shares the Worker's address.
The instance's pins come from its last checkpoint, and only the forks it made
of withheld trackers are looked at; the container asks whether each pin is on
that tracker's `main`, so an instance merely behind `main`, or already merged,
does not count. A fork no checkpoint describes counts as work. After `newgit
merge <instance>` the redelivered event recognises the instance's commits and
brings in only the mirror's merge, which keeps the edit: a merge takes a
withheld tracker's files from whichever parent is ahead.

**When it fails.** A failure (GitHub or Artifacts down, the container gone)
is logged and tried again, five times. If root's ref is not what `main`'s
index names, or the index moved under the pull, it starts over from what
`main` is now rather than retrying a base that cannot work. A wait, on
someone else's merge or on that, gives up after two hours. After that it is dropped, loudly, and redelivering the
event from GitHub's *Recent Deliveries* starts it again. Everything is in
the Worker's logs under `webhook`. GitHub's own page shows only the 202.

**What it trusts.** GitHub, to say who pushed what: the signature proves an
event came from someone holding the secret, and the payload is taken at its
word. There is one secret per remote, so setting up a webhook is an
operator's act; a repo's owner cannot do it alone. Landing on `main` skips
`newgit merge`'s own checks of pin moves, because no pin moves but by the
split itself, and it skips no maintainers' check. It does not re-verify
GitHub's signatures on commits, and a branch's protection rules are GitHub's
to enforce: anyone GitHub lets onto that branch, who is also a member here,
can land work on `main`. What lands is exactly what is on the mirror's
branch at the event's commit.

**A pull request's commits** come in as new root commits, not as the
instance's own: those are in the instance's forks, which the pull does not
read. The instance's work is already on `main` then, so merging it finds
nothing to do but a merge commit.

**Only `main`.** The webhook lands on `main`, not the project's
`defaultBranch`, because an export composes `main` and the container moves
that ref: the rest of the path would have to follow first.

**Setting one up**, in the repo's *Settings → Webhooks*:

- Payload URL:
  `https://newgit.dev/v1/webhooks/github?project=<owner>/<name>&target=<target>`
- Content type: `application/json`
- Secret: the value of `GITHUB_WEBHOOK_SECRET`, set with `wrangler secret put
  GITHUB_WEBHOOK_SECRET`
- Events: *Just the push event*

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

A preview is one end of reviewing a change: everything runs in the sandbox.
The general case, where some of a change runs on the reviewer's machine, is
below.

### Reviewing a change: each tracker runs here or there *(decided, planned)*

A reviewer wants to try a change against their own environment — their
data, their env, the roles they normally bind — with only what the change
touched swapped in. So a review is decided per tracker the change touched:
does it run **here** (on the reviewer's machine) or **there** (in the
preview sandbox)? This settles [#229] ("preview = role replaced by container
remote?"): what a preview replaces is not a role but where a tracker's
resources run.

- **The branch is a trial merge.** The review's branch is your `main` with
  the change merged in, as `newgit merge` does it: one merge commit, so
  trackers the change did not touch stay at `main`'s pins. Since `--only`
  was dropped, a trial merge takes every tracker the change touched; a
  change that should be tried apart goes in its own instance.
- **Clients run here.** A tracker whose resources are local processes the
  reviewer drives (an Electron app, a CLI, a mobile build) can only be tried
  on the reviewer's machine: an Electron-only change is a local trial merge
  and nothing more, against your usual backend.
- **Services can run there.** A tracker whose resources export URLs (a
  database, an API, a web server) can be *placed* in the preview: the
  tracker is bound at the change's commit as usual, but its resources run in
  the sandbox, and their exports (`SUPABASE_URL`, `DATABASE_URL`) render into
  the reviewer's files as the preview's addresses, the way ports render
  today. The role, its `requires`, and binding are untouched; only placement
  changes. A placed resource is in effect `external`: another system owns
  it.
- **A web-only change needs nothing here:** open the preview.

So a change touching `supabase` and `electron` runs `supabase` in the
preview and `electron` locally, pointed at it — something like `newgit spawn
review --from <instance> --remote supabase` (the command shape is not
settled). The reviewer also sees the data the author's agent actually
produced, not a fresh database, because the preview's database is shared
(*One sandbox per distinct environment*).

**Networking.** HTTP to a placed resource goes through the Worker, with the
same per-request auth as any preview request. Raw TCP (a direct Postgres
connection) needs newgit to run a local forwarder tunnelled through the
Worker, so the rendered `DATABASE_URL` is a `127.0.0.1` port newgit carries,
like `cloudflared access tcp`.

**Missing before any of this works:** `merge --into` a throwaway branch and
`spawn --from` a newgit branch rather than a Git revision; resource
placement and the TCP forwarder; and previews themselves.

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
   repository unchanged as a tracker in Git ([#140]), export a tracker as its
   own root ([#141]), pull a target's new commits back into its trackers
   ([#142]).
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
   Reviews that place a change's services in the preview and run its
   clients locally, through a trial merge (*Reviewing a change*, [#229]).
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
8. **Worker in TypeScript; CLI stays Rust.**
9. **ProjectDO is the source of truth** for members, branches, instances,
   and checkpoints when a remote is configured. Binding records stay local.
10. **Artifacts is the project remote** in remote mode; GitHub is an export
    (`newgit export --remote`), and what is merged there comes back through
    newgit (*The GitHub webhook*), never by fetching GitHub.
11. **Identity is GitHub login**, via device flow. **Agents act as their
    owner.**
12. **Tracker history is Git** (`refs/trackers/<tracker>/*`), in a tracker repo
    separate from the store.
13. **A review decides each touched tracker's placement:** a trial merge of
    the change onto `main`, with clients run locally and services
    optionally run in the preview, their exports rendered locally. A preview
    replaces where a tracker's resources run, never a role ([#229]).

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
  on its copy (*Forks*): minutes for t3code.
- **The 1 GB repo cap** for large `db-snapshots` trackers.
- **Merges inside an instance's work are flattened** when it is split into
  trackers ([trackers.md](trackers.md), *Build order* step 5).
- **Networks that filter `newgit.dev`** reset TLS; explain it, and consider
  a fallback host ([#130]).

Known gaps, tracked: [#224] (per-user `main` reaches the remote only
through a checkpoint), [#107] (the push outbox is never garbage-collected),
[#130] (TLS resets), [#228] (a definition claim can lock the owner out).

---

## Measurements

The spikes behind this design (2026-10-04 and 05) and the first live run
are kept in full, dated, in
[remote-measurements.md](remote-measurements.md). The numbers the design
leans on:

| What | Measured | So |
|---|---|---|
| Artifacts fork (hono, 41 MB) | 5–22 s; never finished on t3code (365 MiB) | fork only the trackers an instance changes, and copy them with Git (*Forks*) |
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
[#144]: https://github.com/Spheroman/newgit/issues/144
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
