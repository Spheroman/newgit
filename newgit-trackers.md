# Trackers all the way down

*Design for newgit's core model: no privileged Git source. Every tracker is
its own repository; a workspace is composed from the trackers it binds; Git
and GitHub see a projection. Supersedes the v1 split between "source" and
"other trackers". Decisions marked **(decided)** are made; **(open)** ones
are listed at the end.*

---

## The model in one page

- **A tracker** is a definition in `.newgit/trackers/<name>.toml`: the paths
  it owns, who may read it, which resources it brings, and which role (if
  any) it fills.
- **A tracker's repo** is its history: an ordinary Git repository with a
  `main` branch (what new instances get) and one branch per instance that
  has changed it. On the remote, it is that tracker's Artifacts repo.
  *("Lane" is retired as a term: it is just the tracker's repo.)*
- **A workspace** is composed from the trackers an instance binds: each
  tracker's files at the instance's commit, laid out by path. To agents and
  tools it is one ordinary Git repository.
- **A resource** is brought by a tracker and runs what that tracker's content
  needs (a database, a dev server, an install).
- **The project** is `.newgit/` itself — definitions only — held by the one
  tracker every instance binds, `project`.

```
            trackers (each its own repo: main + instance branches)
   project ─┐   root ─┐   web ─┐   electron ─┐   local-db ─┐   jack-env ─┐
            ▼         ▼        ▼             ▼             ▼             ▼
         ┌───────────────────────────────────────────────────────────────┐
 bind →  │ workspace: files composed by path · .git = projection          │
         │ resources: those the bound trackers bring                      │
         └───────────────────────────────────────────────────────────────┘
            ▲ capture splits commits back into each tracker's repo by path
```

---

## Tracker definitions

```toml
# .newgit/trackers/electron.toml
paths = ["apps/electron"]
audience = "project-devs"        # public | project-devs | user | user:<login>
priority = -1                    # negative: bound only when asked
resources = ["electron-dev"]

# .newgit/trackers/local-db.toml
role = "db"                      # trackers sharing a role are alternatives
paths = [".env.db"]
resources = ["postgres"]

# .newgit/trackers/staging-db.toml
role = "db"
paths = [".env.db"]
priority = 10
```

| Field | Meaning |
|---|---|
| `paths` | What it owns. The most specific owner of a path wins, so `root` can own `.` while `electron` owns `apps/electron`. |
| `audience` | Who may read its repo. Enforced by the remote (token minting). |
| `role` | Trackers with the same role are mutually exclusive alternatives: an instance binds exactly one per role. A tracker without a role is its own slot. |
| `priority` | Which alternative is picked by default: highest readable wins. **Negative: never picked automatically** — only by `--use` or a personal setting. |
| `resources` | Resources this tracker brings. A resource is bound iff a bound tracker brings it. |
| `requires` | Trackers or roles this one needs: bound with it, and merged before (or with) it. |
| `secret` | Never in the workspace's Git: excluded, never committed, captured by newgit from the files at checkpoint. The default; `secret = false` makes a code tracker. |
| `maintainers` | Who may merge changes to it: `owner`, `project-devs`, or `user:<login>`; by default its audience. See *Who may change what*. |
| `merge` *(planned)* | `with-change` (merged together with whatever else a change touched) or `explicit` (only by `newgit tracker merge`; env files). Would replace `merge_with_source`. |
| `production` *(planned)* | Production content: agent and instance tokens may not bind it, and its resources' destructive hooks are refused. |

**Ownership (decided).** Every path has exactly one owner: the most specific
claim among the project's trackers. Two trackers may not claim the same path
unless one's claim is strictly more specific — except trackers in the same
role, which may claim identical paths, since they are never bound together.

**The root tracker (decided).** `newgit init` creates `root`, owning `.`. It
holds whatever no other tracker claims — in a fresh project, everything — so
a project never has unowned files. Carving out a tracker (`newgit tracker
create electron --code --path apps/electron`) moves those paths' history out of
`root` into the new repo (a path-filtered split, done once).

---

## Binding

**What a binding is (decided).** An instance's history holds every code
tracker its user may read, whatever it binds — so every instance of a project
has the same commit ids. Binding is what the instance *works on*: a dropped
code tracker's files are left out of its working tree (Git's sparse checkout)
and its work is out of scope — nothing splits into it, and a merge leaves it at
the branch's commit. A dropped secret tracker's files are removed after its
content is captured. Neither ever makes a commit.

At spawn, for each slot (each role, and each role-less tracker):

1. `--use <tracker>` or `--drop <tracker>` first.
2. Otherwise, for a role, your own default: `[projects.<project>.roles]` in
   `~/.newgit/config.toml` — yours, never written by newgit, and it overrides
   priority.
3. Otherwise the highest-priority tracker in the slot (ties broken by name),
   if its priority is ≥ 0. A role-less tracker is bound unless its priority is
   negative.
4. Then whatever the bound trackers `require`, transitively — an unfilled
   required role is a spawn error naming the trackers that could fill it — and
   the resources the bound trackers bring.

`newgit tracker use <tracker>` binds a tracker into an existing instance:
swapping out whatever filled its role, binding what it requires, and starting
the resources it brings. `newgit tracker drop <tracker>` unbinds one, stopping
(never cleaning up) the resources no longer brought; it is refused while
another bound tracker requires it, and while the tracker has uncommitted work
unless `--stash` (brought back by the next `use`) or `--discard` (after a
checkpoint, so `newgit undo` gets it back). `root`, `project`, and `access`
cannot be dropped. A checkpoint records the binding, and undo restores it.

`{{bound.trackers}}` and `{{bound.paths}}` give a resource's commands what the
instance binds, so a shared install builds only the bound members.

**Dependencies (decided, not wired yet).** `depends_on` names a resource or a
role. A role dependency is satisfied when the role is filled: if the bound
tracker brought resources, the dependent waits for them; if it brought none
(`staging-db`), its content alone satisfies it.

---

## The workspace is a projection

Agents expect one Git repository, and newgit lies to them about it, on
purpose, without emulating Git: the workspace's `.git` is a real repository
whose commits newgit composes.

**Tracker repos are the truth; a workspace's Git is a view (decided).** The
remote holds one repo per tracker, with its own permissions. A machine
composes the code trackers its instance binds (and its user may read) into
one ordinary Git history — the view — in the project repo, and workspaces
clone that as they always have. Work leaving the machine is split back into
per-tracker commits. Two people's views can differ: each composes only what
its user may read. Tracker repos store files at their workspace paths
(`apps/electron/…`), so composing is overlaying trees; ownership guarantees
they never collide.

**Composing is full replay (decided).** Every bound code tracker's history
is read in one `git log --raw` pass (first parent, diff against it), all
trackers' commits are interleaved by committer date (ties: tracker name,
then commit id), and the whole view is written in one `git fast-import`
stream, applying each commit's changes to the running tree. View commits
carry the original author, date, and message — nothing added — so `git log`
and `git blame` are real everywhere. Which tracker commits a view commit
came from is kept in a note (`refs/notes/newgit-composed`). Composing again
after trackers move appends to the view; it rewrites the view only past
the earliest commit whose position changed.

**Merges keep every parent (decided).** Replay covers each tracker's whole
history, not just its first-parent line: the commits a merge brings in are
replayed as real view commits, and a rebuilt merge points at them, so `git
log` shows merged branches and `git blame` reaches into them. On a side
branch, a tracker's commit carries the other trackers' content as it stood
where the branch forked, plus any of its own changes to paths another bound
tracker owns (pre-carve history, the true state on that branch).

**Originals are reused (decided).** A view commit is emitted as the existing
original — same object, id, merge parents, signature — whenever it lands
exactly where the original sits (same parents, each reused itself) with
exactly its changes. A history that never diverged across trackers is reused
whole, and nothing is written; recreating starts only where tracker
histories really differ.

**Splitting (decided).** A view commit is split by ownership into one commit
per code tracker it touched, on that tracker's instance branch: same author,
date, and message, plus a `Newgit-Split: <view commit>` trailer. On replay,
tracker commits sharing a split source fold back into one view commit with
the trailer stripped — so a commit split and recomposed is *the same commit*:
same tree, parent, author, date, and message, same id. Commit ids never
change under an agent, on any machine.

**One commit, one audience (decided).** Because every part of a split keeps
the commit's message, a commit that changes code trackers with different
audiences would put a message about work only some may read into a tracker
everyone else reads ("Fix the refund bypass" landing in `root`). The split
refuses it, naming the commit and each tracker's audience: make one commit
per audience. A refusal, not a warning, because the split is where work
leaves the machine, so a warning would arrive after the message had gone.
Merge commits are exempt (their message is Git's own, and their first-parent
diff spans whatever the merged branch changed); history already split is not
checked again, so a teammate's published work never blocks your sync.

**Signed commits (decided).** A signature covers a commit's exact content,
so a recomposed commit cannot carry the original's. Three rules:

1. **Locally, the original is reused.** When a group would recompose into
   content identical to the view commit it was split from (same parent,
   same changes), compose emits that original object — signature and id
   intact. An agent's own workspace never sees its commits change.
2. **Split commits are signed as their author** when the author configured
   signing (`commit.gpgSign`): newgit signs the per-tracker commits on that
   person's machine, with their key.
3. **The limit, stated plainly:** a teammate composing your trackers, or a
   GitHub export, receives tracker commits, not your combined commit. Their
   composed commit is newly made and unsigned; your tracker commits stay
   signed by you.

**Splitting happens when work leaves the machine (decided):** sync, push,
merge. Local history is left alone, and local undo works on the view.

**The project root is the instance `main` (decided).** Before a spawn, the
project root's commits not yet split reach each tracker's `instances/main`.
Its own branch — the one checked out at `newgit init` — is the truth for
that instance: a `git reset` of it moves those branches back, while a
feature branch or tag checked out in the project root is not its history.
No spawned instance may be named `main`. A refusal there is a warning, not an error — nothing has
left the machine — and the split runs again before anything does.

**Composing decides ownership among code trackers (decided).** A secret
tracker is never in a view, so a path it owns now that history once
committed (a `.env`, later gitignored) stays in the history of the code
tracker that held it then. Dropping it would rewrite every commit after
it; removing it is a history rewrite (`git filter-repo`), which is not
newgit's to do silently.

**Carving out a tracker** (`newgit tracker create electron --code
--path apps/electron`) splits its paths' history out of root the same way,
each commit marked with its source, so the view reproduces the original
history exactly — mixed commits included. Every branch of the source is
carved — `main` and every instance's — and every view is recomposed, so
no instance is left with notes that predate the new tracker. Split refuses
rather than guess if one ever is.

**Objects: every repository references what it needs (decided).** No
borrowing between repositories (Git alternates): Git's own cleanup in a
repository deletes whatever *that* repository does not reference, so
borrowed history can vanish weeks later. Root's history is copied into the
tracker store at `newgit init`; split commits are fetched into the store's
refs once made; composing fetches code trackers' commits into the project
repo under its own refs. Secret trackers never enter the view: their files
are projected into workspaces as today.

**Unbound trackers are out of the working tree, not the history (decided).**
A tracker the instance does not bind is hidden by Git's sparse checkout:
`tracker use` widens it, `tracker drop` narrows it, and history never changes
(see *Binding*). A tracker its *user* may not read is absent from the history
altogether.

**Secrets never leave through Git (decided).** A commit adding or changing
a secret tracker's file (`git add -f .env.local`) is refused when it would
leave the machine, naming the commit and the file, even by force. There is
one gate, run over exactly what is sent — every commit, every parent of a
merge, so a secret added and deleted on a merged side branch is still
caught: a push checks every ref it sends (any branch, any tag) against
what the remote already has; a checkpoint sync checks everything it
uploads, its uncommitted-work commit included. Deleting a secret file is
how it comes out, so it is never refused. Splitting itself only assigns
paths to trackers.

**New work only (decided):** root's `main` — the history newgit adopted —
is exempt, so an old committed `.env` never blocks a first push to a fresh
remote. A secret committed *after* `newgit init` is refused by newgit's
remote even when plain Git already pushed it elsewhere: it has to come out
of the history (`git filter-repo`).

**Writes outside the binding (decided).** A change to a path whose owner is
not bound (an agent creates `apps/electron/x` in an instance without
`electron`) refuses the capture and says so: `apps/electron belongs to
electron, which this instance has not bound — newgit tracker use electron`.

**Rendered values** (ports written into files by resources) are reversed
before splitting, as capture does today, so no tracker's history ever
records one instance's ports.

---

## Installs across trackers

Trackers decide which manifests exist; resources build environments from
them; the package manager resolves. newgit never merges package lists or
resolves versions — that would be emulating npm or pip.

- **A package's own manifest** (`apps/electron/package.json`,
  `apps/ml/requirements.txt`) lives in its tracker, which brings the install
  resource for it. Separate environments cannot conflict.
- **One shared lockfile** (a pnpm/npm workspace, a uv workspace's `uv.lock`)
  is owned by whoever owns it — usually root — and lists every member, bound
  or not. Its install resource installs only the bound members (`pnpm install
  --filter …`, `uv sync --frozen --package …`) through a `{{bound…}}`
  template variable naming the bound trackers' paths, and the bound set is
  part of the install's identity, so copy-on-write installs are keyed
  correctly. (Step 4.)
- **Python environments are flat**, so code sharing one venv needs one
  resolution — the shared lockfile, where conflicts surface when locking,
  not at spawn.
- **A conflict** fails the install's `prepare` with the package manager's own
  message, blocks dependents (as v1 does), and breaks only the combination of
  bound trackers that conflicts: swapping one out returns to an environment
  already built.

## Checkpoints, merges, and coherence

**A checkpoint** names one commit per bound tracker (plus resource state). It
is the atomic unit across trackers: spawning or undoing a checkpoint restores
every tracker to the commit it names.

**Merges stack by tracker (decided).** Trackers declare what they require:

```toml
# .newgit/trackers/electron.toml
paths = ["apps/electron"]
requires = ["supabase"]      # trackers or roles; a client assumes its backend
```

From an instance's changes, you choose which trackers' changes to merge
(`newgit merge <instance> --only supabase`); the rest are left on the
instance's branches, not lost. A choice is valid when, for every tracker it
merges, every tracker it requires *that also changed in this instance* is
merged too:

- `supabase` alone — fine; the electron changes stay unmerged.
- `supabase` and `electron` — fine; `main`s move in requirement order, all or
  nothing.
- `electron` alone while `supabase` also changed — refused, naming the
  requirement. If `supabase` did not change here, `electron` merges alone:
  its requirement is already met by `main`.

Commits are split per tracker, so a commit that touched both does not pin
them together: the requirement graph, not commit boundaries, decides what may
be separated. An instance spawned from another (`spawn frontend --from
backend`) stacks the same way — its changes merge after, or with, the ones it
was built on. Every merge moves the affected `main`s only after all of them
succeeded: the remote's ProjectDO serializes merges and flips them together.

**`requires` governs binding too.** An instance cannot bind `electron`
without `supabase` (or, for a role, some tracker filling it): `newgit tracker
use electron` brings in what it requires, and dropping a tracker others
require is refused, naming them. Trackers have `requires` (trackers or
roles); resources have `depends_on` (resources or roles) — one model.

`explicit` trackers (env files) are never carried by `newgit merge`; they move
only by `newgit tracker merge <tracker>`.

---

## On the remote

**A branch is a list (decided).** A newgit branch names one commit per code
tracker; `main` is the default branch, as on GitHub. A checkpoint is a frozen
list and an instance is a list being worked on, so spawning starts an
instance from a branch's list. Lists live in the project's Durable Object,
and moving a branch is one write: every tracker moves together or none does.
`newgit remote add` creates `main` from the project root's current list, its
first publication; after that `main` moves only by `newgit merge` (step 5).
`newgit branch create <name> [--from <instance>]` and `newgit branch default
<name>` manage the rest. (A newgit branch is not the Git branches an agent
makes inside one workspace.)

| Repo (Artifacts) | Holds | Read | Write |
|---|---|---|---|
| `<owner>.<project>.t.<tracker>` | a code tracker's history | its audience | seeding, then merges |
| `<owner>.<project>.i.<instance>.<tracker>` | an instance's branch of one code tracker, forked the first time the instance changes it | the tracker's audience; only the instance's owner when it is private | the instance |
| `<owner>.<project>.t.<tracker>[.<login>]` | a secret tracker, as before | its audience | its audience |

No trunk repo, no composed repo, no whole-source fork per instance: spawning
forks nothing.

**Private instances (decided).** `newgit spawn <name> --private` makes an
instance that only its owner sees on the remote: its record, its
checkpoints, and its forks answer everyone else as though it did not exist,
the project's owner included. That is an embargo: a security fix is built,
checkpointed, and spawned elsewhere by its author without the rest of the
project seeing it, and `newgit merge` is the moment it is disclosed. It needs
a newgit remote; without one, `--private` is refused, since a `git push`
to a plain Git remote would publish it. Its name is not hidden: creating
another instance with the same name says it exists.

**The Git endpoint (decided).** `https://newgit.dev/<owner>/<project>.git` is
an ordinary Git remote: `git clone` and `git pull` work with plain Git, no
newgit installed. It advertises each newgit branch as `refs/heads/<branch>`.
The Worker authenticates the caller and hands the request to the project's
sandbox, which runs real `git` and the `newgit` binary: it fetches the
tracker repos, composes the branch's list from the trackers *this caller*
may read, and answers with `git upload-pack`. A workspace's `origin` fetches
from it, so `git pull` shows `main`. Each person's view holds only what they
may read, enforced by the server; nothing is pre-built, so nothing is
rebuilt on merge. The sandbox snapshots its tracker repos and composed views
after each fetch, so a cold start restores rather than re-clones. Read-only
in step 3d: pushes go through a newgit workspace, which splits locally.

Why a sandbox and not the Worker: a Worker cannot run Git, and composing in
JavaScript would mean writing Git's object model ourselves.

**Who may change what (decided).** A tracker's `maintainers` may merge
changes to it; by default they are its audience — anyone who may read a
tracker may change it. Who may change a tracker's *security* (its paths,
audience, maintainers) is then just who may change the tracker that owns its
definition file, `.newgit/trackers/<name>.toml`. That is `access` — which
`newgit init` creates with `maintainers = "owner"` — unless a more specific
code tracker claims the file. So governance layers with no special case:
project trackers; an admin tracker owning their definitions (readable by
everyone who uses them, since newgit needs their paths, but maintained by
admins); and a tracker of yours owning the admin tracker's. Everything else
in `.newgit/` — resources, scripts, config — is the `project` tracker, which
anyone who merges may change. The remote reads each definition from the
tracker that owns its file, at the default branch, to a fixed point.

**One name for a commit, whoever reads it (decided).** A Git commit id is the
hash of the commit's contents and its parent's id, so someone whose view
leaves out a tracker sees different ids from the first commit that touched
it onward. Two layers keep that from mattering:

- **Usually it never happens.** Code trackers are readable by every member
  by default, and everyone who reads the same code gets the same commits.
  A code tracker with a narrower audience needs an explicit flag, whose
  warning names both costs: its past stays readable in root's history, and
  people who cannot read it see different commit ids.
- **When it does, every commit has a canonical id:** the commit as its author
  made it, which every tracker commit split from it already carries as
  `Newgit-Split: <id>`. The endpoint notes it on each rebuilt commit (`git
  log --notes` shows it), newgit commands accept either id, and newgit's own
  surfaces — checkpoints, branch lists, the web UI — always show canonical
  ids. A branch's state as a whole is named by its list.

## Git and GitHub

**GitHub is an export (decided).** `newgit export --github <repo>` pushes a
composed projection of the `public` (or chosen) trackers' `main`s, with
composed history, to an ordinary GitHub repository. GitHub never holds a
tracker the export did not include, so `.env` files can live in the project
and never reach a public mirror.

**Adopting an existing repository (decided).** `newgit init` in a Git
repository imports its full history as `root`'s history. Trackers carved out
later take their paths' history with them.

---

## Build order

Each step lands alone and keeps the suite green.

1. **Tracker repos with history.** Lanes become real branches (`main`,
   `instances/<instance>`) with parents, messages, authors. Capture commits;
   merge merges. The env-file case keeps working unchanged in behavior.
2. **`root` tracker; ownership by specificity; `secret`.** *(Landed.)*
   `newgit init` writes `trackers/root.toml` (owns `.`, committed) and
   copies the project's history into root's `main` with a real fetch (no
   alternates; see *Objects*). Trackers nest; the most specific owner wins.
   `secret` defaults to true.
3. **The projection, and the `project` tracker.**
   - *3a — compose/split engine. (Landed.)* Full replay, merges keep every
     parent, originals reused, split refusals for secret and unbound paths.
   - *3b — code trackers and the `project` tracker. (Landed.)* `newgit
     tracker create <name> --code --path <p>` carves paths out of their
     owner, history included; `project` owns `.newgit`. A code tracker's
     paths are set at creation — adding one later would splice a second
     history into it.
   - *3c — wiring. (Landed.)* The project root's commits split into
     `instances/main` at spawn; push and checkpoint sync split an
     instance's work into `instances/<instance>` and refuse secrets on
     every ref sent; code trackers without history are carved the first
     time they are needed; split commits are signed as their author when
     `commit.gpgSign` is set; `newgit log <tracker> [instance]`. Two
     things wait for later steps: spawn composes nothing yet, because with
     every code tracker bound the view of `HEAD`'s state *is* `HEAD` (the
     composing path arrives with `use`/`drop` in step 4); and split
     follows the first parent, so a merge *inside* the work being split is
     flattened (step 5, with merges).
   - *3d — remote. (Landed.)* Projects as `<owner>/<name>`, branches as
     lists, one Artifacts repo per code tracker, per-tracker forks made
     when an instance changes a tracker, checkpoints synced as lists and
     composed by `spawn --from-checkpoint`, `newgit clone`, and the Git
     endpoint at `newgit.dev/<owner>/<name>.git` (a per-project container
     composing each reader's view). Rebuilt commits carry a note naming
     their canonical id.
4. **Roles, priority, resources, use/drop.** *(Landed.)* An instance's
   history holds every code tracker its user may read; binding is a sparse
   checkout plus scope: `newgit tracker drop` hides a tracker's files and
   takes its work out of the instance (refused with uncommitted work unless
   `--stash` or `--discard`), `use` brings them back. `role`, `priority`
   (negative: only when asked), `requires`, and `resources` decide what a
   spawn binds (`--use`, `--drop`; personal defaults in
   `~/.newgit/config.toml` override priority). `{{bound.trackers}}` and
   `{{bound.paths}}`; `tracker list` as a tree. Resource `depends_on` naming a
   role is not wired yet.
5. **Coherent merges** locally and on the remote: `newgit merge` writes a
   branch's new list in one step — per tracker, the instance's commit when
   `main` has not moved, a real Git merge when both sides changed it —
   with `--only` stacking checked against `requires`. *(Landed, without
   `--only`, which waits for `requires` in step 4. `newgit merge main`
   merges the project root, which is how trackers made after `remote add`
   reach the remote. Merges inside the work being split are still
   flattened.)*
6. **GitHub export.** *(Landed.)* `newgit export --remote <name>` pushes
   the composed `main`s to the target in `.newgit/exports/<name>.toml`:
   `public` code trackers plus the ones it names, so each target has its
   own view. A history adding a secret tracker's path is refused; a push
   that is not a fast-forward needs `--force`. Targets live under
   `project`, so whoever may change resources may change what a mirror
   shows; moving them under `access` waits until that matters.

Then the preview work in `newgit-cloudflare.md` continues on top: a preview's
environment identity is simply the set of tracker commits it composes.

---

## Decided with Jack (2026-10-04)

1. **The project root is a workspace:** the instance `main`, bound to every
   tracker's `main`. You work there as today; merges land there.
2. **Secret trackers stay out of the workspace's Git; code trackers are
   tracked.** *(Revised the same day.)* The first answer was to track
   everything, on the grounds that every push goes through newgit — but only
   pushes to `origin` do, so an agent pushing anywhere else would carry a
   committed `.env` with it. A tracker marked `secret = true` (env trackers
   by default) is excluded from the workspace's Git as in v1: never in a
   commit, captured by newgit from the files at checkpoint, so no push can
   carry it. Code trackers are tracked and split into their repos on
   capture. Limit, stated plainly: an agent can read anything in its
   workspace, so this prevents accidental leaks, not a malicious agent.
3. **Merges stack by tracker**: choose which trackers' changes to merge;
   a tracker's changes merge only with the changes of the trackers it
   `requires` (see *Checkpoints, merges, and coherence*).
4. **The video leads with spawning anything, anywhere** — an agent's attempt
   on your machine or as a preview, with its whole environment and your
   keys — with trackers shown as how it works.
5. **Full replay** for composing the view, rather than root-as-spine: real
   `git log` and `git blame` everywhere, and stable commit ids across a
   split.
6. **Splitting happens when work leaves the machine** (sync, push, merge),
   never at a local checkpoint.
7. **The project root is an instance like any other:** its commits reach a
   shared tracker's `main` only through `newgit merge main`.
