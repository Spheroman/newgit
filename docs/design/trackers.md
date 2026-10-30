# Trackers all the way down

*Design for newgit's core model: no privileged Git source. Every tracker is
its own repository; a workspace is composed from the trackers it binds; Git
and GitHub see a projection. Supersedes the v1 split between "source" and
"other trackers". Decisions marked **(decided)** are made; what is still
open or unbuilt is listed at the end, under *Open questions and known gaps*.
Every definition key is specified in
[definitions.md](../../crates/newgit/reference/definitions.md), which wins
where this document differs; [spec.md](spec.md) is the main design doc, and
[remote.md](remote.md) the newgit.dev remote.*

---

## The model in one page

- **A tracker** is a definition in `.newgit/trackers/<name>.toml`: the paths
  it owns, who may read it, which resources it brings, and which role (if
  any) it fills.
- **A tracker's repo** is its history: a `main` branch (what new instances
  get) and one branch per instance that has changed it. Locally every
  tracker shares one store, `.newgit/local/trackers.git`, under
  `refs/trackers/<tracker>/` (`main`, `instances/<instance>`, and
  `pins/<rev>`, which keep commits a binding or checkpoint names alive);
  only on the remote is each tracker its own repository, an Artifacts repo.
  *("Lane" is retired as a term: it is just the tracker's history.)*
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
# .newgit/trackers/electron.toml — a code tracker
audience = "project-devs"        # public | project-devs | user | user:<login>
storage = "remote"               # local | remote
secret = false
paths = ["apps/electron"]
priority = -1                    # negative: bound only when asked
resources = ["electron-dev"]

# .newgit/trackers/local-db.toml — secret, the default
audience = "project-devs"
storage = "local"
role = "db"                      # trackers sharing a role are alternatives
paths = [".env.db"]
resources = ["postgres"]

# .newgit/trackers/staging-db.toml
audience = "project-devs"
storage = "remote"
role = "db"
paths = [".env.db"]
priority = 10
```

A definition with a missing required key or an unknown one is refused.
[definitions.md](../../crates/newgit/reference/definitions.md) is the
authoritative list; in short:

| Field | Meaning |
|---|---|
| `audience` | Required. Who may read its repo. Enforced by the remote (token minting). |
| `storage` | Required. `local` or `remote`: where its synced state lives. |
| `paths` | What it owns. The most specific owner of a path wins, so `root` can own `.` while `electron` owns `apps/electron`. |
| `role` | Trackers with the same role are mutually exclusive alternatives: an instance binds exactly one per role. A tracker without a role is its own slot. |
| `priority` | Which alternative is picked by default: highest readable wins. **Negative: never picked automatically** — only by `--use` or a personal setting. |
| `resources` | Resources this tracker brings. A resource is bound iff a bound tracker brings it. |
| `requires` | Trackers or roles this one needs: bound with it, and (planned) merged before or with it. |
| `secret` | Never in the workspace's Git: excluded, never committed, captured by newgit from the files at checkpoint. The default; `secret = false` makes a code tracker. |
| `maintainers` | Who may merge changes to it: `owner`, `project-devs`, or `user:<login>`; by default its audience. See *Who may change what*. |
| `merge_with_source` | Whether a source merge carries this tracker's bound state with it. Default `false`. |
| `merge` *(planned)* | `with-change` (merged together with whatever else a change touched) or `explicit` (only by `newgit tracker merge`; env files). Would replace `merge_with_source`. Not accepted today. |
| `production` *(planned)* | Production content: agent and instance tokens may not bind it, and its resources' destructive hooks are refused. Not accepted today. |

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
the resources it brings. A secret tracker comes back with the content it had
when it was dropped, or, if the instance never bound it, with its main, as a
spawn would bind it; anything already sitting at its paths is captured to its
instance branch first. `newgit tracker drop <tracker>` unbinds one, and
*parks* the resources no longer brought (#254, #255): each is taken down as
`newgit stop` would take it down (its process signalled, its `suspend` run),
or, with no `suspend`, by its `[cleanup]`, and the output names the hook that
ran. A parked binding keeps its ports — ports are for the instance's
lifetime — so the next `use` hands them back and runs `resume` (or `prepare`,
when the drop cleaned it up): the files it renders, its data, and whatever
it left behind all agree on them. `remove` tears down parked bindings too.
Shared (`project`, `user`) resources are parked but left up. A drop is
refused while
another bound tracker requires it, and while the tracker has uncommitted work
unless `--stash` (brought back by the next `use`) or `--discard` (after a
checkpoint, so `newgit undo` gets it back). Both rebuild every bound install
whose store key the change moves (see *Installs across trackers*): the tree the
instance holds was built for the binding it had, so it is filled from the store
when that binding was built before, else its `prepare` runs again. Binding a
secret tracker, or one with no paths, moves no install's key unless the
install is told the tracker names through `{{bound…}}`. A rebuild
that fails leaves the resource `failed` without failing the command, since the
binding has changed by then. A process already running when a use or drop
rewrites a rendered file is restarted if its resource declares `reads`
covering that file, and otherwise named as possibly stale, with the command
that restarts it (#249; see *Render* in [spec.md](spec.md)). A secret
tracker's content is captured into its lane before its files leave the
workspace; a tracker with no `paths` (one that only receives `into_tracker`
deposits, or only brings resources) has nothing in the workspace, so there is
no capture step and `--stash`/`--discard` have nothing to act on — its
deposits are already in its lane, and a checkpoint covers them. (An explicit
`newgit tracker capture` of such a tracker is still an error: asked for
directly, it should say there is nothing to capture.) `root`, `project`, and
`access` cannot be dropped. A checkpoint records the binding, and undo
restores it; `newgit status` lists the trackers the binding binds, not every
tracker in a role.

`{{bound.trackers}}` and `{{bound.paths}}` give a resource's commands what the
instance binds, so a shared install builds only the bound members.

**Dependencies (decided; roles not wired yet).** A resource's `depends_on`
names resources or trackers today. Naming a role is the planned addition: a
role dependency would be satisfied when the role is filled — if the bound
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
date, and message, plus trailers: `Newgit-Split: <view commit>`;
`Newgit-Split-Parent: <commit>` naming the commit it followed in the view, so
trackers' commits interleave back in the order they were made rather than
merely by date; and a `Newgit-Split-Header:` for each line of the headers
Git stored after the committer line (a signature, a merged tag, an
encoding), verbatim. One trailer per line, continuation lines included: a
PGP signature holds a line that is only a space, which Git's trailer parser
reads as blank. The split commit's message ends the way the original's did,
final newline or none (GitHub writes merges without one).
On replay, tracker commits sharing a split source fold back into one view
commit with the trailers stripped — so a commit split and recomposed is
*the same commit*: same tree, parent, author, date, message, and headers,
same id. Commit ids do not change under an agent, on any machine,
with two exceptions today: split follows the first parent, so a merge
*inside* the work being split is flattened and its commit recomposes
differently; and a history imported from outside the project is
interleaved by date rather than folded back (#139).

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
so it can only ever sit on the commit it was made for. Three rules:

1. **Locally, the original is reused.** When a group would recompose into
   content identical to the view commit it was split from (same parent,
   same changes), compose emits that original object — signature and id
   intact. An agent's own workspace never sees its commits change.
2. **Elsewhere, the original is rebuilt, signature included.** A composer
   with only the tracker commits — the remote, a teammate, `spawn
   --from-checkpoint` — rebuilds each view commit (Git's `fast-import`
   writes no signature), then puts back the headers the trailers recorded
   and keeps the result only if it hashes to the `Newgit-Split` id. The id
   is the proof: a matching commit is byte for byte the one its author
   made, so its signature verifies with `git verify-commit` like any other,
   and the commits after it move onto it. Whoever can read every tracker a
   commit touched gets the same ids as the project's own history, GitHub
   included. Someone who cannot gets a different tree, so the id cannot
   match, and their commit stays rebuilt and unsigned: a signature is never
   put on content it does not cover. The same holds for commits the forge
   makes when it merges: unsigned ones rebuild exactly, and ones it signs
   carry their signature in the trailer like anyone's.
3. **Split commits are signed as their author** when the author configured
   signing (`commit.gpgSign`): newgit signs the per-tracker commits on that
   person's machine, with their key.

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
paths to trackers. The limit, stated plainly: an agent can read anything in
its workspace, so this prevents accidental leaks, not a malicious agent.

**New work only (decided):** root's `main` — the history newgit adopted —
is exempt, so an old committed `.env` never blocks a first push to a fresh
remote. A secret committed *after* `newgit init` is refused by newgit's
remote even when plain Git already pushed it elsewhere: it has to come out
of the history (`git filter-repo`).

**Writes outside the binding (decided).** A change to a path whose owner is
not bound (an agent creates `apps/electron/x` in an instance without
`electron`) is refused when the work is split — when it leaves the machine,
not at a local checkpoint — and says so:

```
`apps/electron/x` belongs to `electron`, which this instance has not bound: `newgit tracker use electron`
```

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
  template variable naming the bound trackers' paths. What of the binding
  can reach the install is part of its identity, so copy-on-write installs
  are keyed correctly: which paths the checkout leaves out, and the bound
  set itself when a `{{bound…}}` tells the install it. (Step 4.)
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

**What is built.** `newgit merge [instance] [--into <branch>]` merges an
instance's committed work into a branch (`main` by default), tracker by
tracker: per tracker, the instance's commit when the branch has not moved, a
real Git merge when both sides changed it, and every tracker moves together
or none does — on the remote, a compare-and-swap in the ProjectDO.
`newgit merge main` merges the project root. Choosing a subset of trackers,
and checking `requires` when you do, is the design below, not built yet.

**Merges stack by tracker (decided, planned).** Trackers declare what they
require:

```toml
# .newgit/trackers/electron.toml
paths = ["apps/electron"]
requires = ["supabase"]      # trackers or roles; a client assumes its backend
```

From an instance's changes, you would choose which trackers' changes to merge
(`newgit merge <instance> --only supabase`, planned); the rest are left on the
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
be separated. An instance spawned from another would stack the same way —
its changes merge after, or with, the ones it was built on. *(Planned:
today `spawn --from` takes a Git revision, not an instance, so nothing
records that one instance was built on another.)* Every merge moves the
affected `main`s only after all of them succeeded: the remote's ProjectDO
serializes merges and flips them together.

**`requires` governs binding too (built).** An instance cannot bind
`electron` without `supabase` (or, for a role, some tracker filling it):
`newgit tracker use electron` brings in what it requires, and dropping a
tracker others require is refused, naming them. Trackers have `requires`
(trackers or roles); resources have `depends_on` (resources or trackers;
roles planned) — one model.

Env-file trackers move by `newgit tracker merge <tracker>`, which promotes
an instance's revision to the tracker's `main`. Today `merge_with_source`
decides whether a source merge carries a tracker too; the planned `merge`
key would replace it, and `explicit` trackers would never be carried by
`newgit merge`.

---

## On the remote

**A branch is a list (decided).** A newgit branch names one commit per code
tracker; `main` is the default branch, as on GitHub. A checkpoint is a frozen
list and an instance is a list being worked on, so spawning starts an
instance from a branch's list. Lists live in the project's Durable Object,
and moving a branch is one write: every tracker moves together or none does.
`newgit remote add` creates `main` from the project root's current list, its
first publication; after that `main` moves only by `newgit merge` (step 5).
The remote already lists branches, creates one from a list, and sets the
default (`/branches` routes in `remote/src/index.ts`); the CLI to match,
`newgit branch create <name> [--from <instance>]` and `newgit branch default
<name>`, is planned. (A newgit branch is not the Git branches an agent makes
inside one workspace.)

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
rebuilt on merge. When the sandbox goes idle, an alarm snapshots its tracker
repos and composed views (if anything was composed since the last snapshot)
and then stops it, so a cold start restores rather than re-clones. Read-only
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
  A code tracker with a narrower audience should need an explicit flag,
  whose warning names both costs: its past stays readable in root's
  history, and people who cannot read it see different commit ids.
  *(Planned: today `tracker create --code --audience` takes any audience
  without a warning. What is built is the split's refusal of one commit
  spanning audiences, under* One commit, one audience*.)*
- **When it does, every commit has a canonical id:** the commit as its author
  made it, which every tracker commit split from it already carries as
  `Newgit-Split: <id>`. A commit given back as its original (*Signed
  commits*, rule 2) is its canonical id. The endpoint notes it on each
  commit that stays rebuilt, in
  `refs/notes/newgit-canonical`, served to Git clients as
  `refs/notes/commits` so `git log` shows it; newgit commands accept either
  id, and newgit's own
  surfaces — checkpoints, branch lists, the web UI — always show canonical
  ids. A branch's state as a whole is named by its list.

## Git and GitHub

**GitHub is an export (decided).** `newgit export --remote <name>` pushes a
composed projection of the `public` (or chosen) trackers' `main`s, with
composed history, to the target defined in `.newgit/exports/<name>.toml` —
an ordinary GitHub repository, say (step 6). GitHub never holds a
tracker the export did not include, so `.env` files can live in the project
and never reach a public mirror.

**Adopting an existing repository (decided).** `newgit init` in a Git
repository imports its full history as `root`'s history. Trackers carved out
later take their paths' history with them.

---

## Build order

Each step lands alone and keeps the suite green. Status as of 2026-10-28.

1. **Tracker repos with history.** *Done.* Lanes became real branches
   (`main`, `instances/<instance>`) with parents, messages, authors, in the
   shared store described under *The model in one page*. Capture commits;
   merge merges.
2. **`root` tracker; ownership by specificity; `secret`.** *Done.*
   `newgit init` writes `trackers/root.toml` (owns `.`, committed) and
   copies the project's history into root's `main` with a real fetch (no
   alternates; see *Objects*). Trackers nest; the most specific owner wins.
   `secret` defaults to true.
3. **The projection, and the `project` tracker.**
   - *3a — compose/split engine. Done.* Full replay, merges keep every
     parent, originals reused, split refusals for secret and unbound paths.
   - *3b — code trackers and the `project` tracker. Done.* `newgit
     tracker create <name> --code --path <p>` carves paths out of their
     owner, history included; `project` owns `.newgit`. A code tracker's
     paths are set at creation — adding one later would splice a second
     history into it.
   - *3c — wiring. Done.* The project root's commits split into
     `instances/main` at spawn; push and checkpoint sync split an
     instance's work into `instances/<instance>` and refuse secrets on
     every ref sent; code trackers without history are carved the first
     time they are needed; split commits are signed as their author when
     `commit.gpgSign` is set; `newgit log <tracker> [instance]`. A local
     spawn never composes: an instance's history holds every code tracker,
     so the workspace is the project's own history and binding is a sparse
     checkout (step 4). Composing happens where a history has to be built
     from tracker repos: `spawn --from-checkpoint`, `newgit clone`, `newgit
     export`, the Git endpoint, recomposing every view after a carve, and
     bringing the project root along after a merge into `main`.
     Split follows the first parent, so a merge *inside* the work being
     split is still flattened (step 5).
   - *3d — remote. Done.* Projects as `<owner>/<name>`, branches as
     lists, one Artifacts repo per code tracker, per-tracker forks made
     when an instance changes a tracker, checkpoints synced as lists and
     composed by `spawn --from-checkpoint`, `newgit clone`, and the Git
     endpoint at `newgit.dev/<owner>/<name>.git` (a per-project container
     composing each reader's view). A rebuilt commit is given back as its
     original, signature included, when its id comes out as the
     original's; one that cannot be carries a note naming its canonical
     id. Tracker commits split before the trailers kept message endings
     and headers never come back as the original (step 3e).
   - *3e — repairing older splits. Done.* Tracker commits split before
     the trailers recorded headers and message endings lost both, so on
     the remote they rebuild with new ids, and so does every commit after
     them. `newgit tracker repair` rewrites each such commit's trailers
     from the original view commit, where this machine has it (trees,
     authors, and committers unchanged; a commit whose original is not
     here keeps its message and only moves; a carved commit's trailers
     follow the tracker commit it was carved from when that moves too).
     Rewritten tracker commits are unsigned, so the same repair always
     makes the same ids and a retry after a partial failure lands on what
     the first attempt pushed; the commits they compose into keep their
     signatures (*Signed commits*). It then moves every code tracker
     branch, the composed-state notes, and on the remote each branch's
     list, the way a merge moves it: its tracker repos force-pushed with a
     lease. Instance forks and checkpoints on the remote keep the commits
     they were made with, so an old checkpoint composes as it always did.
     A force push of tracker history, so it runs once, by hand
     (`--dry-run` says what would move).
4. **Roles, priority, resources, use/drop.** *Done, except a resource's
   `depends_on` naming a role.* `role`, `priority` (negative: only when
   asked), `requires`, and `resources` decide what a spawn binds (`--use`,
   `--drop`; personal defaults in `~/.newgit/config.toml` override
   priority); `newgit tracker use` and `drop` change it afterwards (see
   *Binding*). `{{bound.trackers}}` and `{{bound.paths}}`; `tracker list` as
   a tree.
5. **Coherent merges** locally and on the remote. *Partial.* Built:
   `newgit merge [instance] [--into <branch>]` writes a branch's new list
   in one step — per tracker, the instance's commit when the branch has not
   moved, a real Git merge when both sides changed it — moved by
   compare-and-swap on the remote; `newgit merge main` merges the project
   root, which is how trackers made after `remote add` reach the remote.
   Missing: `--only` and checking it against `requires`; spawning one
   instance on another and stacking their merges; merges inside the work
   being split kept rather than flattened; the `newgit branch` CLI.
6. **GitHub export.** *Done.* `newgit export --remote <name>` pushes
   the composed `main`s to the target in `.newgit/exports/<name>.toml`:
   `public` code trackers plus the ones it names, so each target has its
   own view. A history adding a secret tracker's path is refused. Before
   pushing, it brings in what was merged on the target (a pull request
   merged on GitHub): the project root fast-forwards to it and it merges
   into `main`. When the target's commits are not the project's own, it
   refuses and says how to bring them in; `--force` replaces the branch
   instead, and `--dry-run` composes and checks without pushing. Targets
   live under `project`, so whoever may change resources may change what a
   mirror shows; moving them under `access` waits until that matters.
   With `branches = true`, each workspace push exports the instance's
   branch too, and on a newgit remote the target is the workspace remote
   `gh` resolves: interpose, don't emulate ([vision.md](vision.md)) — the
   tracking ref is the commit the target holds, and when that is not the
   agent's `HEAD` (a filtered target), the push says `gh pr create
   --head`.

Then the preview work in [remote.md](remote.md) continues on top: a
preview's environment identity is simply the set of tracker commits it
composes.

---

## Decided with Jack (2026-10-04)

The rest of that day's decisions — secret trackers out of the workspace's
Git, merges stacking by tracker, full replay, splitting when work leaves the
machine — are recorded where they apply above. Two add something:

1. **The project root is a workspace:** the instance `main`, bound to every
   tracker's `main`. You work there as today; its commits reach a shared
   tracker's `main` only through `newgit merge main`.
2. **The video leads with spawning anything, anywhere** — an agent's attempt
   on your machine or as a preview, with its whole environment and your
   keys — with trackers shown as how it works.

---

## Open questions and known gaps

Unbuilt parts of the design above are marked *planned* where they appear:
the `merge` and `production` keys, `merge --only` and its `requires` check,
spawning an instance on another, the `newgit branch` CLI, the narrow-audience
warning, and `depends_on` naming a role. Open issues on Spheroman/newgit:

- **Imported histories.** Compose interleaves trackers by committer date,
  which is right for carved trackers but wrong for a history from outside
  the project; mount it through one merge commit instead (#139). Importing
  an outside repository unchanged as a code tracker, `tracker create --code
  --from` (#140). Exporting a tracker as its own root, for a fork that keeps
  its files at the top (#141). Pulling a target's new commits back into its
  trackers, `newgit pull --remote` (#142). Resolving `gh` to the export
  target an instance's work belongs to, not the first `branches = true` one
  (#143).
- **Per-user trackers.** A per-user tracker's `main` reaches the remote only
  as a side effect of some instance's checkpoint (#224).
- **Sparse checkouts.** Restoring a dirty checkpoint writes files into a
  dropped tracker's sparse-excluded paths (#225).
- **Governance.** A tracker handed one definition can claim `access`'s and
  lock the owner out (#228). No orgs: owners are always logins, and
  `project-devs` is the only group (#227).
- **Resources.** Nothing orders `start`, so a runtime dependency (functions
  needing supabase running) cannot be expressed (#54). Whether the
  `preview` role gives way to a container remote (#229).
- **The model itself.** Dropping projects as a structure: every tracker the
  same kind of thing, rooted at the user (#201).
