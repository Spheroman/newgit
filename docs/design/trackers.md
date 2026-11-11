# Trackers all the way down

*Design for newgit's core model: no privileged Git source. Every tracker is
its own history, stored relative to its own root; a parent records the
commit of each tracker it mounts; the root tracker's history is the view's
shape; Git and GitHub see a projection. Supersedes the v1 split between
"source" and "other trackers", and folds in the roots-and-mounts design
decided with Jack on 2026-11-04 (built as #201). Decisions marked
**(decided)** are made; what is still open or unbuilt is listed at the end,
under *Open questions and known gaps*. Every definition key is specified in
[definitions.md](../../crates/newgit/reference/definitions.md), which wins
where this document differs; [spec.md](spec.md) is the main design doc, and
[remote.md](remote.md) the newgit.dev remote.*

*Terms: a tracker **in Git** (`git = true`) is committed with Git and split
into its history; one **outside Git** (`git = false`, the default) is
captured by newgit and never committed.*

---

## The model in one page

- **A tracker** is a history of files stored relative to its own root, and a
  definition in `.newgit/trackers/<name>.toml`: where it is mounted, what it
  claims, who may read and maintain it, the resources it brings, which role
  (if any) it fills, and whether it is in Git.
- **A mount** places a tracker at a path inside another tracker.
- **A pin** is the commit of a mounted tracker in Git that a parent commit
  holds, stored in the parent's tree as a Git *gitlink* (the object Git uses
  for a submodule's commit) at `.newgit/pins/<name>`.
- **A root** is the tracker an instance is rooted at: today, always `root`.
  The instance's `.git` is the *view*: the root's history with every pin
  expanded into files under its mount, recursively.
- **A view commit is one root commit.** Each commit of the root's history
  composes to exactly one view commit, and splitting a view commit makes
  exactly one root commit. The root's history is the view's shape.
- **A tracker's repo** is its history: a `main` branch (what new instances
  get) and one branch per instance that has changed it. Locally every
  tracker shares one store, `.newgit/local/trackers.git`, under
  `refs/trackers/<tracker>/` (`main`, `instances/<instance>`, and
  `pins/<rev>`, which keep commits a binding or checkpoint names alive);
  only on the remote is each tracker its own repository, an Artifacts repo.
  *("Lane" is retired as a term: it is just the tracker's history.)*
- **A workspace** is a clone of the view with the trackers the instance does
  not bind hidden by sparse checkout. To agents and tools it is one ordinary
  Git repository.
- **A resource** is brought by a tracker and runs what that tracker's content
  needs (a database, a dev server, an install).
- **`.newgit/` is ordinary content of the root tracker.** `access`, mounted
  at `.newgit/trackers`, holds the definitions: who may read what.

```
   root                                   electron
   ────                                   ────────
   R3  split from V3, pins electron at E2  ◄──  E2   (V3's electron half)
   R2  newgit pull electron: pins X1       ◄──  X1   (merged on electron's main)
   R1  carves electron, pins E0           ◄──  E0   (the files at its own root)

   view rooted at root:
     V3 ── V2 ── V1 ── root's history before the carve …
     (V2 holds electron's files at X1, and nothing of X1's history)
```

A submodule in storage, a monorepo in every view. Agents never see a
gitlink, `.gitmodules`, or a nested `.git`.

---

## Tracker definitions

```toml
# .newgit/trackers/electron.toml — a tracker in Git
audience = "project-devs"        # public | project-devs | user | user:<login>
storage = "remote"               # local | remote
git = true
mount = "apps/electron"          # where its root sits in the parent; default "."
follow = "main"                  # the ref `newgit pull electron` brings in; default "main"
paths = ["."]                    # claims, relative to the mount
priority = -1                    # negative: bound only when asked
resources = ["electron-dev"]

# .newgit/trackers/local-db.toml — outside Git, the default
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
| `mount` | Where its root sits inside the parent that defines it. Default `.`: an overlay beside the parent's files. |
| `follow` | The ref `newgit pull <tracker>` brings in. Default `main`. |
| `paths` | Its claims, relative to the mount. `!` excludes: `[".", "!dist/"]`. The most specific owner of a path wins (see *Ownership*). |
| `role` | Trackers with the same role are mutually exclusive alternatives: an instance binds exactly one per role. A tracker without a role is its own slot. |
| `priority` | Which alternative is picked by default: highest readable wins. **Negative: never picked automatically** — only by `--use` or a personal setting. |
| `pinned` | Whether its parent holds it where it is (docs/design/anchoring.md, *Follow and pinned*): a change landing at it never rolls up into the parent, and moving the parent's pin is a change to the parent, approved by the parent's maintainers; rollback is a revert. For what must not move by itself: a private dependency, `/env/prod`. Default `false`. |
| `link` | Where it lives when it lives in another project: `"<owner>/<project>[/<path>]"` (see *Links*). Its history, heads, repo, and policy are its home's; only where it is mounted is this project's. Only a tracker in Git. Written by `newgit tracker link`. |
| `resources` | Resources this tracker brings. A resource is bound iff a bound tracker brings it. |
| `from` | The tracker whose history this one started at: a fork, whose audience may not be wider than its upstream's. Written by `newgit tracker create --from`. See *Binding*. |
| `requires` | Trackers or roles this one needs: bound with it, and not droppable while it is bound. |
| `git` | Whether its content is committed with Git. `false` (the default): never in the workspace's Git, excluded, never committed, captured by newgit from the files at checkpoint. `true`: a tracker in Git, committed with Git and split into its own history. Written only when `true`. |
| `maintainers` | Who may merge changes to it: `owner`, `project-devs`, or `user:<login>`; by default its audience. See *Who may change what*. |
| `merge_with_source` | Whether a source merge carries this tracker's bound state with it. Default `false`. |
| `merge` *(planned)* | `with-change` (merged together with whatever else a change touched) or `explicit` (only by `newgit tracker merge`; env files). Would replace `merge_with_source`. Not accepted today. |
| `production` *(planned)* | Production content: agent and instance tokens may not bind it, and its resources' destructive hooks are refused. Not accepted today. |

There is one kind of tracker. `git` says only where its content lives: in
the workspace's Git, or outside it where newgit captures it. It says nothing
about how sensitive the content is; that is the `audience`. A `.env` is a
tracker outside Git, and so are agent notes, a db snapshot, and the public
`site` page. Fail-closed is why outside Git is the default: a new tracker
never lands in a commit by accident.

**Where a definition lives (decided).** A tracker's children are defined in
its own `.newgit/trackers/`. Root's live in `access`, which is mounted there;
before `access` is carved (the commits before the first merge of a fresh
project) they are in root's own tree. A mount is relative to the parent's
root, so nested mounts work through this: a tracker mounted at
`apps/electron` defines its own children in `apps/electron/.newgit/trackers/`,
mounted relative to `apps/electron`.

**The root tracker (decided).** `newgit init` creates `root`, owning `.`. It
holds whatever no other tracker claims — in a fresh project, everything,
`.newgit/` included — so a project never has unowned files. Carving out a
tracker in Git (`newgit tracker create electron --git --path apps/electron`)
is one commit; see *Carving is a commit*.

## Ownership

Every path in a view has at most one owner. Ownership comes from claims,
never from which tracker currently has the file, and it is the same whichever
tracker you root at. **(decided; rule 1 as of the #426 review, 2026-11-11)**

1. **The longest claim that reaches a path owns it.** A claim reaches a path
   under it unless a tracker mounted deeper holds the path and the claim is
   `.`, or a directory around that mount, or the claiming tracker is in Git.
   So only a tracker outside Git reaches into another's mount, with a claim
   inside it: `paths = ["apps/t3code/.env"]` owns that file just as `mount =
   "apps/t3code"`, `paths = [".env"]` would.
2. **A tracker's own `!` exclusions take it out of the running** for what
   they exclude: `paths = [".", "!.env", "!node_modules/"]`. A path no
   reaching claim takes is owned by nothing, which is all "ignored" ever
   meant, and a commit changing it is refused; a resource may fill it.
3. **Identical claims are an error, except between role alternatives**, of
   which a workspace binds one.

What follows from rule 1: a path under `apps/electron/` is decided among
the trackers mounted at `apps/electron` or deeper, and trackers outside Git
with a claim inside it. Trackers sharing a mount split by their claims:
`local-db` mounted at `.` claiming `.env.db` beats root's `.`, and a tracker
mounted beside `access` at `.newgit/trackers` claiming `web.toml` owns that
file while `access` keeps the rest. A claim of a tracker in Git inside a
deeper mount owns nothing (`newgit tracker list` says so): `pub`, in Git,
claiming `apps/t3code/secrets/pub.txt` while `t3code` is mounted at
`apps/t3code` does not take that file from `t3code`, nor from `sec`, outside
Git with audience `user`, mounted there claiming `secrets`. A tracker in Git
that should own something inside another's mount is carved there, as a
child.

Why trackers in Git and outside it differ. Split puts a tracker in Git's
files in its own commits by mount: a deeper mount is cut out of the trees of
the trackers around it. A claim of a tracker in Git reaching into one would
be a promise split cannot keep: the file would land in the tracker mounted
there, or in its parent. A tracker outside Git's files are never split: a
commit adding one is refused, and claiming inside a mount is exactly how
`newgit tracker track` writes a `.env` (`paths = ["apps/electron/.env"]`).
Judging that claim by mount (before the #426 review) let a target's commit
adding the file through the check; letting a claim of a tracker in Git
reach in too (the first #426 review fix) let a commit move a file of `sec`
into root's `main`.

**One function decides.** `owners_of` (`crates/newgit-core/src/tracker.rs`)
is this rule. The commit and push gate, split's check, export, import,
pull, and the workspace (capture, checkout, clear: what
`resolve_ownership` settles for each tracker) all ask it. Split assigns a
path a commit adds to the tracker in Git it names, or refuses the commit
when it names a tracker outside Git, or nothing; a generated test
(`tests/ownership_agrees.rs`) holds all three to agreeing. Of role
alternatives tied on one claim, the gate names one outside Git first, so
the file is refused whichever one a workspace binds. The server's
copies (`newgit-serve`'s `policy.rs`, the Worker's `ownerOf`) decide among
trackers in Git only, for definition files under `.newgit/trackers/`, where
no tracker outside Git may be mounted: there the deepest mount holding the
path decides, then the longest claim, which is this rule.

A tracker carved inside another tracker's mount (`apps/electron/native`
while `electron` is mounted at `apps/electron`) is a child of `electron`
**(decided 2026-11-06, replacing "a deeper mount of root's")**: `electron`
defines it in its own `.newgit/trackers/`, mounted relative to itself, and
its commits pin it, so it goes wherever `electron` goes. Rooted at `electron`
the view still holds those files. Defined by root instead, the only commit
that could say which pin went with which of `electron`'s commits was root's,
and `electron`'s repository, exported on its own, lost the files.

Who may change that definition is who may change `electron`: the file is
`electron`'s, by the same rule as any definition. A name `access` has a file
for is never taken by a nested definition, even if that file does not parse,
and names are unique within a parent. Only a tracker in Git is defined
inside another, and it needs a `mount` of its own.

**Landing:** newgit.dev's Worker lands at anchors and rolls parents up
([anchoring.md](anchoring.md), as built). `newgit serve` replaces that with
one-transaction landings and one object store per audience
([landing.md](landing.md), decided, not built).

**A tracker is named by its path (decided 2026-11-06).** A name is local to its
parent, as a file's is to its directory: `repos.toml` and `.newgit/pins/repos`
in `t3code`'s own tree, so that tree says nothing of where `t3code` sits. Two
parents can each have a `docs`. Commands read a path as a filesystem does: from
the tracker the working directory is in, `..` going up, and from the top of the
one namespace with a leading `/`, which is `/remote/<owner>/<project>` for a
project on a remote and `/user/<project>` for one that is not. A name is never
looked for further up, so it means one thing wherever it is typed.

What refs, records, and remote repos hold is an id, `t3code__repos`, with no
slash: `refs/trackers/<tracker>/...` holds the tracker's own branches, so a
child at `t3code/instances` would land among them, and a remote repo's name
has one part per field. An id names the parent, so the remote reads a nested
definition from that one tracker, and no other can answer for it. `__` is
reserved, and a name cannot start or end with `_`, so two paths never make one
id. People never type an id.

**A child exists only in its parent (decided 2026-11-06).** `native` is bound
only where `electron` is: dropping `electron` drops it and says so, and asking
for `native` by name brings `electron`. Where the parent is bound, the child's
own `priority` decides as usual: `-1` keeps it out until asked for, and
anything else brings it along, on `tracker use electron` too. So a large
directory can stay out of workspaces that want its parent, and with both at
`-1`, `tracker use electron` brings only `electron`. A tracker root defines
deep inside another is not its child, and stays when what surrounds it goes.

**The definition is the `.gitignore`.** Generating the view's `.gitignore`
from claims, and turning an agent's edit of it into a definition change, is
planned (step 7). Compose doesn't depend on it.

## Shape and policy

Compose needs the **shape** of each commit: which trackers it pins, at which
commits, mounted where. Everything that decides who may read or change
something is **policy**. They come from different places, on purpose
**(decided)**:

- **Shape comes from the commit itself.** Its pins, and each pinned tracker's
  `mount` as that commit's own tree states it (in `access`'s content at that
  commit). A commit that moves a mount composes by the new one.
- **Policy comes from `access`'s own `main`,** never through a pin: who may
  read and maintain each tracker. The Worker already reads definitions this
  way. So `access`'s pin in the root exists only so that views reproduce. No
  one can roll policy back by moving a pin, or by editing an old commit.
- **Claims are read only by split.** Compose never reads them: each tracker's
  stored tree contains exactly what it owned when the commit was made, so
  overlaying the stored trees is enough. Changing a claim (adding `!dist/`)
  changes where new work goes, never what old commits look like.

---

## Binding

**What a binding is (decided).** An instance's history holds every tracker
in Git its user may read, whatever it binds — so every instance of a project
has the same commit ids. Binding is what the instance *works on*: the files of
a dropped tracker in Git are left out of its working tree (Git's sparse
checkout) and its work is out of scope — nothing splits into it, and a merge
leaves it at the branch's commit. The files of a dropped tracker outside Git
are removed after its content is captured. Neither ever makes a commit.

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
the resources it brings. A tracker outside Git comes back with the content it had
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
tracker outside Git, or one with no paths, moves no install's key unless the
install is told the tracker names through `{{bound…}}`. A rebuild
that fails leaves the resource `failed` without failing the command, since the
binding has changed by then. A process already running when a use or drop
rewrites a rendered file is restarted if its resource declares `reads`
covering that file, and otherwise named as possibly stale, with the command
that restarts it (#249; see *Render* in [spec.md](spec.md)). The content of a
tracker outside Git is captured into its history before its files leave the
workspace; a tracker with no `paths` (one that only receives `into_tracker`
deposits, or only brings resources) has nothing in the workspace, so there is
no capture step and `--stash`/`--discard` have nothing to act on — its
deposits are already in its history, and a checkpoint covers them. (An explicit
`newgit tracker capture` of such a tracker is still an error: asked for
directly, it should say there is nothing to capture.) `root` and `access`
cannot be dropped. A checkpoint records the binding, and undo
restores it; `newgit status` lists the trackers the binding binds, not every
tracker in a role.

`{{bound.trackers}}` and `{{bound.paths}}` give a resource's commands what the
instance binds, so a shared install builds only the bound members.

**Members of a role are forks (decided 2026-11-08, #201; built for trackers
outside Git, #415).** Project branches own the branch axis, so a variant
cannot be a branch of a tracker (`db@staging/feature-x` would stop the names
lining up); it is a member of a role. What members lacked was a shared
history to merge along, and a fork gives them one:

```
newgit tracker create staging-db --role db --from local-db
```

- **Its `main` is `local-db`'s very commit.** Locally that is two refs in the
  one store (`main` and a pin), so nothing is copied. Its definition says
  `from = "local-db"`, claims what `local-db` claims at its mount, and fills
  its role (`--role` names one when `local-db` has none, and is written to
  `local-db` too). It brings no resources and requires nothing, and its
  `priority` is below `local-db`'s and negative: making a fork never changes
  what anything binds by default.
- **A fork can't be wider than its upstream (decided).** Its history holds
  everything its upstream had, so it may be read by no one the upstream's
  audience does not include: the same audience, or one inside it (`public`
  holds every audience, `project-devs` holds `user` and `user:<login>`, which
  the remote serves to members only; any other two are not ordered). The
  ceiling belongs to the history, not to the definition's `from` line, so
  dropping or changing `from` lifts nothing. Where it is held:
  - *The client* refuses it at creation, naming both audiences. After that,
    every command but `tracker list` and `tracker remove` (which can fix it)
    refuses while a fork's definition breaks it, and says what to type: a
    fork wider than its upstream, one whose upstream is not defined, forks in
    a cycle, or one whose `from` was dropped or changed while its audience is
    wider than what it started at. What it started at is this machine's
    record, `refs/newgit/forks/<fork>/<upstream>` in
    `.newgit/local/trackers.git`, written when the fork is made and whenever
    a definition saying `from` is read. Split refuses any commit whose
    definitions differ from any parent's and break the ceiling, held to its
    parents' forks, every earlier commit's in the same push, and those
    records; a merge's result is checked whole, since one side adding a fork
    and the other narrowing its upstream are each fine alone. The upstream
    can't be removed while a fork names it or is recorded as started at it.
    This is a convenience for the developer, not a boundary: a client can be
    bypassed.
  - *`newgit serve`* is the boundary. When it makes a fork's repo it records
    the upstream as `refs/newgit/from/<upstream>` in the fork's namespace,
    hidden from every Git request so no push moves or deletes it. As it reads
    definitions it holds every fork to both the upstream `from` names and the
    recorded one, until nothing changes, and says so when it refuses a read:
    every rule and every store then sees it no wider. Against each upstream
    a fork within it is left alone, and one the upstream is within is
    narrowed to the upstream's audience; capping only ever narrows. A fork
    whose ceiling can't be settled is not defined (readers are told so): an
    upstream that isn't defined, an upstream it is not ordered with
    (`user:jack` under an upstream narrowed to `user:bob`: moving it would
    hand jack's own commits to bob), a cycle, a chain more than 64 forks
    long, a linked fork whose home can't be read, or two upstreams no one
    audience fits under. Each holds down a chain of forks too.
  - *newgit.dev's Worker* has no record and no cap: only the client checks
    the ceiling there, and a definition landed past it is served as written.
- **A fork is never detached (decided).** Its history stays its upstream's,
  so nothing makes it stop being a fork: to share something more widely,
  make a new tracker (`newgit tracker create <name>`) and capture into it
  only what may be shared. The server's record lives as long as the fork's
  repo does.
- **A narrower fork lives in the narrower store.** On `serve` a fork's repo
  is made on its first sync, starting at its upstream's branches, and is
  checked first: refused, before anything is made or copied, when the fork is
  wider than its upstream or the caller may not read the upstream. With the
  same audience that is a namespace in the upstream's store, refs alone. A
  narrower one is made in its own audience's store, which fetches only what
  the upstream's `main` reaches (the upstream's branches are copied, and a
  tracker outside Git has only `main` today): all of it readable by that
  audience when the fork was checked, so nothing is written into a store
  whose readers could not read it then. The check and the copy are not one
  step: the copy runs after the db lock is let go of, so an upstream
  narrowed, or a caller who loses access, in between leaves those commits
  in the narrower store (`serve.md`, *Known limits*). The copy runs before
  the landing that makes the fork, which
  writes its starting refs and its record of the upstream with its row, so
  a fork is there whole or not at all, and the next write makes one that
  isn't. What the fork gains after that stays in its own store. newgit.dev's Worker takes the history as the
  sync uploads it (a copy).
- **An encrypted upstream (#429).** A fork is encrypted to its own
  audience. One with its upstream's audience starts at its upstream's
  commits as they are. A narrower one starts at them too (its readers could
  read all of it already), and its first sync encrypts its content again to
  its own recipients, in a commit on top: everything it gains is readable
  by its audience alone. The client counts what the remote has of a fork's
  upstream as what it has of the fork, since the fork's repo starts there,
  so that history is never made again. That is safe only because a fork is
  never wider than its upstream: counting a wider fork's upstream history
  as sent would leave readers of the fork history they cannot read. Both encrypt the upstream's paths,
  so the guard on paths that held encrypted content never holds a fork
  back.
- **Only a tracker outside Git can be forked yet.** Every instance's view
  holds every tracker in Git its user may read, so two members in Git at one
  mount would collide; `--git --from <tracker>` says so. Forking one in Git
  at another mount, and `tracker mount <owner>/<tracker>`, build on the same
  `from` later.
- **Next (#201, step 2):** `tracker merge` / `rebase` / `cherry-pick` between
  members, on the current project branch, with `--disclose` to merge into a
  member with a wider audience. Unrelated histories are refused as Git
  refuses them, which is why `--from` is the way to add a member.

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
composes the trackers in Git its user may read into one ordinary Git history
— the view — in the project repo, and workspaces clone that as they always
have. Work leaving the machine is split back into per-tracker commits. Two
people's views can differ: each composes only what its user may read.

### Storage

**Every tracker stores files relative to its own root (decided).**
`apps/electron/x.ts` in the view is `x.ts` in electron's repository. That is
what lets one tracker be mounted by two parents, or be a root itself, and it
is the shape an imported repository already has, so importing rewrites
nothing.

**Pins.** Each commit of a parent holds a gitlink at `.newgit/pins/<name>`
for every tracker in Git that it mounts. Pins are left out of every view, so
no workspace ever holds one. They sit under `.newgit/pins/` rather than at
the mount because a tracker can be mounted at `.` (an overlay beside its
parent's files), and a gitlink cannot be the root of a tree. A gitlink
rather than a text file because it is Git's own object for "this tree holds
that commit". newgit resolves pins itself in every merge, so it never relies
on Git's handling of gitlinks. Gitlinks don't make Git keep the commit; the
tracker store's refs do.

**Trackers outside Git may have a `mount`** but are never pinned. Their state
stays in the instance's binding record and in checkpoints.

### Snapshots

A view needs a child's files at each commit root pins, never the child's
history, so `newgit clone` fetches root's whole history and each commit it
pins `--depth=1`, by id. `--full` fetches every history up front instead,
for someone who knows they will work in a child.

- **The project repo is the shallow store.** It is where composing and
  splitting run, and it has to be able to push, so it holds a real `shallow`
  file: Git then treats a pin as a commit with no parents everywhere, `gc`
  and `fsck` included. `refs/newgit/snapshots/<tracker>/<commit>` keeps each
  pin alive and says whose it is. This differs from the remote's
  `compose.git` + `snapshots.git` ([remote.md](remote.md), *Fetching what a
  view needs*) for a reason: `compose.git` reads through an alternate and
  never fetches, and the project repo fetches all the time. A store that
  fetches must not have a shallow alternate.
- **`trackers.git` is never shallow**, so a tracker held as snapshots has no
  refs there until its history arrives. A workspace is a clone of view
  commits, none of which reach a child commit, so it is whole. A project
  with a `shallow` file is cloned over Git's pack transport rather than by
  hardlinks, which costs the first spawn a copy of the view.
- **A tracker is held as snapshots while one of its snapshot refs is in
  `shallow`.** There is no second record to disagree with Git, and a project
  that never had snapshots is never shallow, so a full clone and every
  project cloned before this behave as they always did.
- **A snapshot does not deepen by accident.** `fetch_refs` carries on
  deepening a store left shallow by a stepped fetch; it ignores commits
  under `refs/newgit/snapshots/`, so an ordinary fetch leaves them alone. A
  fetch into the project stops at a shallow commit, so bringing `main` on top
  of a pin costs its new commits. Bringing any other branch of a tracker
  would cost its whole history, so a project holding snapshots fetches only
  the branches a command asks about.
- **A commit already here is never fetched `--depth=1`**: root's own history
  is fetched first, and a commit root shares with a child stays whole. A
  shallow commit that later lands on root's history would end it, so root's
  walk checks for one and fetches that tracker's history.
- **A new pin arrives before it is composed.** Composing leaves out a pin it
  cannot find, so before composing, `bring_pins` fetches every snapshot root's
  history lacks.

- **A remote may refuse a push from a shallow repo.** `git push` names every
  entry in `shallow`. When one that looks refused fails, every tracker held as
  snapshots gets its history, which empties the file, and the push runs once
  more, saying so. Pushing from the outbox instead is no way round it: it
  borrows these objects through alternates without the `shallow` file.

**History arrives in one place, `need_history`.** It fetches the tracker
whole into `trackers.git`, where a dropped fetch steps and resumes as any
whole fetch does, and `--unshallow`s the project from there, which deepens
only what that store knows, so every other tracker stays a snapshot. It says
`fetching t3code's history from newgit.dev; this happens once`, and it does.

The rule for what calls it: Git's "yes" is right on a snapshot, since a commit
reachable from another is an ancestor however short the walk. Its "no" means
nothing. An instance commit under a mount needs only its pin as a parent, so
split, checkpoint, push, and an ordinary merge fetch no history.

| Asks about history | What happens on a snapshot |
|---|---|
| A child's `main` moved past the pin, and the merge has to join them | Asks both ways round; neither → `need_history`, then `git merge-tree` |
| A split where a view merge joins two lines of a child, with a tree equal to one of them | Asks whether that one descends from the other; "no" → `need_history` |
| `newgit pull <tracker>`, `newgit pull` | Fast-forward: no history, `follow` lands on the pin and is counted. Otherwise `need_history`, so a pin is never moved backwards. A rooted target's tip the pin or `follow` already has needs none either; anything else is `need_history` first, as `--remote` is |
| `newgit pull --remote` from a rooted target, a rooted export, `newgit log <tracker>`, `tracker migrate` | `need_history` first: each walks the history itself |
| `spawn --from-checkpoint` | A plain fetch from the author's fork stops at the pin, so the checkpoint's commits are whole enough to push on; other pins come from the tracker's repo; no history unless root's walk reaches a snapshot |
| A tracker's commits judged before a push | Stops at the pin, which is what the remote already holds |
| Which tracker an instance touched, for where its branch is exported | Asks both ways round; neither → `need_history`. A false "touched" would send the branch to the wrong repo |
| "Is this tracker behind?" at spawn, an export's "unmerged" | Advisory: a false "no" says less or names a tracker it need not. Decides nothing |

### Compose

Compose walks one history, the root's. A child is expanded at each commit
that history pins, and a grandchild at each commit those pin: a child's tree
with its own pins expanded is what a parent mounts. No child's parents are
read, so a store holding root's history and each pinned commit composes the
same ids as one holding every history. The remote relies on this (*Fetching
what a view needs* in [remote.md](remote.md#fetching-what-a-view-needs)).

For a view rooted at tracker `T`, for a reader who may read some set of
trackers **(decided)**:

1. **Walk `T`'s history** in `git rev-list --topo-order --reverse --parents`
   order, every parent of every commit. The order is part of the spec, since
   byte-identical output depends on it.
2. **For each commit `R`, the view commit `V(R)` has:**
   - **parents:** `V(p)` for each parent `p` of `R`, in order, and no others;
   - **tree:** `R`'s tree without `.newgit/pins/`, with each readable pinned
     child's view tree at its pin set under its mount, with no diffing;
   - **metadata:** `R`'s author, committer, dates, and message, newgit's
     trailers stripped and the recorded headers put back. When `R` holds
     none of `T`'s own changes, and a child commit holds the work (see
     *Split*), the metadata comes from that child commit: the child whose
     pin moved at a commit split from a view commit, read as it stands.
3. **A child is its tree at the pin, never its history (decided
   2026-11-05).** A tracker's history stays in its own repo, and no view
   reaches it. Two things can happen at `R`, and both are one view commit:
   - **Work made here.** `R` was split from a view commit, so its child
     commits are the other halves of that work. `V(R)` is that view commit.
   - **History from elsewhere.** `R` adds a mount or moves a pin: an import,
     a `newgit pull`, an upstream sync, a pin moved by hand. `V(R)` is one
     ordinary commit on `V(p)`, its tree holding the child's files at the
     pin. `git log -- apps/electron` starts at the import and shows each
     pull as one commit; the child's own log is in the child
     (`newgit tracker log`, or its repository).

   Nothing depends on the child's commits, so nothing has to be recomputed,
   no two branches can disagree about them, and a mount removed and added
   again costs one commit.
4. **Readers.** A child the reader may not read is left out of the tree, and
   so is everything mounted inside it. A single-parent view commit whose only
   change was to such a child is dropped, and the commit after it moves onto
   its parent. Only a pin of `T` itself counts: a commit moving a child they
   may read, whose move changed only something mounted inside that child,
   stays, with no change to its tree, because telling would take the child's
   history. A merge goes the same way only when its sides differ in such
   a child and what is left adds nothing to one parent: the side that moved
   the pin was dropped, so there is nothing to join. Any other merge stays.
   Someone who cannot read a tracker sees different ids from the first commit
   that touched it (see *One name for a commit*).

   A pin counts if its tracker's repo serves that commit, whether or not a
   branch reaches it **(decided)**: the reader may read that tracker anyway,
   and a merge's check on a pin is unchanged. So a commit force-pushed away
   stays reachable through a pin until the remote garbage-collects it.
5. **Originals are reused.** Commits are written byte for byte, so when
   `V(R)`'s parents, tree, and metadata come out as the view commit `R` was
   split from (its `Newgit-Split`), compose emits that object, signature and
   id intact. A composer without it rebuilds it and puts the recorded
   headers back (#291's mechanism).

**Commits from before mounts.** A root commit whose definitions put
`.newgit/` in a `project` tracker, or leave `access` unmounted, or mount no
tracker in Git at all, pins nothing and is its own view commit. A commit
whose definitions mount a tracker in Git it does not pin is a view commit
that only a split can make a root commit of. This is what lets `newgit init`
adopt a repository's history as root's, and what the migration rests on.

### Split

Splitting view commit `V` (work leaving the machine: push, sync, merge),
rooted at `T` **(decided)**:

1. **Walk every parent,** not only the first. A parent already split is
   found through the note on it (`refs/notes/newgit-composed`: `<tracker>
   <commit>`) or the root commit naming it, so a merge of an already-pushed
   `origin/main` maps to the root commit it was split into. The refusals
   below apply to every commit sent, side branches included.
2. **Assign each changed path** to its owner, by the claims as `V`'s own tree
   states them.
3. **For each child, deepest first:** if `V`'s subtree for it equals the
   child's view at one parent's pin, and that pin descends from every other
   parent's pin, reuse that pin and make no commit. (Merging `main` into an
   instance that never touched electron makes nothing in electron.)
   Otherwise commit the child's part on the child's instance branch, on top
   of the child's pins in `V`'s parents (a merge where they differ), at the
   child's own root.
4. **Commit `R` to `T`** with `T`'s own changes, every pin moved to what step
   3 decided, and parents mapped one-to-one from `V`'s parents. A merge in
   the view is a merge in the root, so merges inside the work are kept, and
   an instance commit touching two trackers stays one commit wherever it is
   merged.

**Split refuses, naming what to do instead:**

- a path whose owner this instance has not bound (`newgit tracker use`);
- a path owned by a tracker outside Git (see *What a tracker outside Git
  owns never leaves through Git*);
- a path nothing owns, because every claim excludes it;
- a path under `.newgit/pins/`, which only newgit writes;
- a commit whose message would be carried by trackers of different
  audiences (next section).

**Which commit carries the message.** `V`'s author, dates, and message go on
the commit that holds the work:

- **`R` holds `T`'s own changes, or no child commit was made** (an empty
  commit, a merge where every child's pin was reused): `R` carries `V`'s
  metadata, with `Newgit-Split: <V>` and a `Newgit-Split-Header:` per
  recorded header (one trailer per line, continuation lines included: a PGP
  signature holds a line that is only a space, which Git's trailer parser
  reads as blank; the message ends the way the original's did, final newline
  or none). Any child commits carry the same author, dates, and message,
  without trailers.
- **`R` holds only child changes** (one child or several): every child commit
  carries `V`'s metadata without trailers, and `R` gets a fixed message
  (`Update electron`), the child commits' dates, and the trailers. Compose
  takes `V`'s metadata from the child commits, which are identical. The
  fixed message never reaches a view.

This keeps every tracker's own history clean: a tracker that mounts nothing
in Git has its commits shown as themselves, and a repository that receives
its history gets ordinary commits, not ones ending in `Newgit-Split:` lines.
It also lets a narrow child be edited from a wider root: the message stays
with the child.

**One commit, one audience (decided).** When more than one commit carries
`V`'s message, all of their trackers must share one audience. Otherwise a
message about work only some may read would land in a tracker everyone else
reads ("Fix the refund bypass" landing in `root`). The split refuses it,
naming the commit and each tracker's audience: make one commit per audience.
A refusal, not a warning, because the split is where work leaves the
machine. Exempt: a merge whose message nobody wrote — Git's own one line
(`Merge branch 'x'`, `Merge remote-tracking branch 'origin/x'`, optionally
`into y`) or newgit's (`Merge `x` into y`); a merge with words of its own is
refused like any commit — and the carve commit newgit makes (`Create tracker `x` at <path>`, which says
nothing the definition it adds does not; without this, carving a
narrow-audience tracker out of root would always be refused). History
already split is not checked again, so a teammate's published work never
blocks your sync.

**Signed commits (decided).** A signature covers a commit's exact content,
so it can only ever sit on the commit it was made for. Three rules:

1. **Locally, the original is reused.** When a root commit and its children
   recompose into content identical to the view commit they were split from,
   compose emits that original object — signature and id intact. An agent's
   own workspace never sees its commits change.
2. **Elsewhere, the original is rebuilt, signature included.** A composer
   with only the tracker commits — the remote, a teammate, `spawn
   --from-checkpoint` — rebuilds each view commit, then puts back the
   headers the trailers recorded and keeps the result only if it hashes to
   the `Newgit-Split` id. The id is the proof: a matching commit is byte for
   byte the one its author made, so its signature verifies with `git
   verify-commit` like any other, and the commits after it move onto it.
   Whoever can read every tracker a commit touched gets the same ids as the
   project's own history, GitHub included. Someone who cannot gets a
   different tree, so the id cannot match, and their commit stays rebuilt
   and unsigned: a signature is never put on content it does not cover. The
   same holds for commits the forge makes when it merges: unsigned ones
   rebuild exactly, and ones it signs carry their signature in the trailer
   like anyone's.
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
No spawned instance may be named `main`. A refusal there is a warning, not
an error — nothing has left the machine — and the split runs again before
anything does.

**Composing decides ownership among trackers in Git (decided).** A tracker
outside Git is never in a view, so a path it owns now that history once
committed (a `.env`, later gitignored) stays in the history of the tracker
in Git that held it then. Dropping it would rewrite every commit after
it; removing it is a history rewrite (`git filter-repo`), which is not
newgit's to do silently.

### Carving is a commit

`newgit tracker create electron --git --path apps/electron` commits the
definition in the project root, and splitting that commit makes one root
commit, `Create tracker `electron` at apps/electron`, that removes the files
from the root's tree and adds the mount and a pin to the child's first
commit, which holds those files at its own root. History before the carve
stays in the root, where the view already shows it, so nothing is rewritten
and no id changes. The cost: rooted at `electron` alone, its history starts
at the carve. Carving with history can come later as an explicit rewrite if
a case needs it **(decided)**.

**Objects: every repository references what it needs (decided).** No
borrowing between repositories (Git alternates): Git's own cleanup in a
repository deletes whatever *that* repository does not reference, so
borrowed history can vanish weeks later. Root's history is copied into the
tracker store at `newgit init`; split commits are fetched into the store's
refs once made; composing fetches the commits of trackers in Git into the
project repo under its own refs. Trackers outside Git never enter the view:
their files are projected into workspaces as today.

**Copying refs between local repos is a ref move when it can be (decided).**
The project repo, `trackers.git`, and each workspace clone hold real copies,
but a copy between two of them usually has nothing to transfer: the store
and the project repo share root's history, and a workspace made the commits
its checkpoint names. `git fetch` finds that out too, but only after
starting `upload-pack`, negotiating, and running auto-maintenance on both
sides, about 25–40 ms each time, and a `spawn` did it several times over.
So `local_refs::sync` asks the destination first, with the check `git
fetch` itself runs before deciding it has nothing to transfer (`rev-list
--objects --stdin --not --all`), and when the destination holds everything
the refs reach it moves them in one `update-ref --stdin`, about 3 ms.
Anything else (a missing object, a shallow destination, a ref the source
lacks) goes to `git fetch` exactly as before. Several refs to one store go
in one call, never one fetch per tracker. Checkpoint (store ← workspace) and
undo (workspace ← store) make the same check for their one commit.

Considered and refused: borrowing through `objects/info/alternates` would
make every copy a ref move, and it is the thing the paragraph above rules
out. The project repo pruning an object the store borrowed loses a
tracker's history weeks later, and a workspace is `rm -rf`'d whenever
someone likes, so nothing may borrow from one. Alternates in the store would
also carry into every local clone of it.

These copies take the refs they name and nothing else, by the fetch too:
`--no-tags --no-prune --no-recurse-submodules`. Before, a fetch
between the project repo and the store followed tags, so a project's tag
landed in `trackers.git` and came back into the project after the developer
deleted it; and a `fetch.prune = true` in someone's Git config would have
deleted the project repo's tracker refs that the store lacks.

**Unbound trackers are out of the working tree, not the history (decided).**
A tracker the instance does not bind is hidden by Git's sparse checkout:
`tracker use` widens it, `tracker drop` narrows it, and history never changes
(see *Binding*). A tracker its *user* may not read is absent from the history
altogether.

**What a tracker outside Git owns never leaves through Git (decided).** This
is the guarantee for secrets: a `.env` belongs to a tracker outside Git. A
commit adding or changing a file such a tracker owns (`git add -f
.env.local`) is refused when it would leave the machine, naming the commit
and the file, even by force. There is one gate, run over exactly what is
sent — every commit, every parent of a merge, so a file added and deleted on
a merged side branch is still caught: a push checks every ref it sends (any
branch, any tag) against what the remote already has; a checkpoint sync
checks everything it uploads, its uncommitted-work commit included.
Deleting such a file is how it comes out, so it is never refused. Splitting
itself only assigns paths to trackers. The limit, stated plainly: an agent
can read anything in its workspace, so this prevents accidental leaks, not a
malicious agent.

**New work only (decided):** root's `main` — the history newgit adopted —
is exempt, so an old committed `.env` never blocks a first push to a fresh
remote. A file of a tracker outside Git committed *after* `newgit init` is
refused by newgit's remote even when plain Git already pushed it elsewhere:
it has to come out of the history (`git filter-repo`).

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
  set itself when a `{{bound…}}` tells the install it.
- **Python environments are flat**, so code sharing one venv needs one
  resolution — the shared lockfile, where conflicts surface when locking,
  not at spawn.
- **A conflict** fails the install's `prepare` with the package manager's own
  message, blocks dependents (as v1 does), and breaks only the combination of
  bound trackers that conflicts: swapping one out returns to an environment
  already built.

## Checkpoints, merges, and pull

**A checkpoint** names one commit per bound tracker (plus resource state). It
is the atomic unit across trackers: spawning or undoing a checkpoint restores
every tracker to the commit it names.

**A merge is a view commit, split like any other (decided).** `newgit merge
<instance> [--into <branch>]` makes the merge commit in the merger's view
(parents: the branch's view tip and the instance's), with each tracker's
files merged by `git merge-tree` (a conflict moves nothing, and names each
file's tracker), and splits it the ordinary way. The root merge carries
`Newgit-Split`, so a merger's signed merge commit comes back as itself. What
is committed in the workspace now is what merges: a checkpoint is taken
first. `newgit merge main` merges the project root, which is how trackers
made after `remote add` reach the remote.

**The commit point is the root's ref (decided).** A branch is a ref on the
root tracker's repository. Its view follows from that one commit, and on the
remote, ProjectDO's list for the branch is an index written after the ref
moves (see *On the remote*).

1. Before anything is pushed, the merge names the index it will make, and
   the Worker asks Git — in the Git endpoint's container, which runs it —
   how each child's pin moves along that child's own history. A move forward
   along the child's `main` changes only root, so root's maintainers may
   make it. Any other move — back, sideways, or to a commit `main` does not
   have — is the child's maintainers' to make, and a commit off `main` gets
   them a write password for the child: on a merge into `main` its `main`
   catches up (step 3); on a merge into another branch the child's commit
   goes to the child's branch of that name, and no child's `main` moves.
   Root always needs its own maintainers. A tracker leaving the index needs
   its maintainers, unless nothing defines it any more (the migration
   dropping `project`); one joining needs whoever governs its definition.
2. The root's ref moves by one compare-and-swap. If it loses the race,
   nothing has changed and the merge reruns on the new tip.
3. Each child's `main` then moves to include what the root now pins, also by
   compare-and-swap. If a child's `main` moved meanwhile (a merge on the
   child's own upstream), newgit merges the two and moves the child's `main`
   to the result. The root picks that merge up at its next `newgit pull`.
   The views are coherent throughout: the root pins exactly what was merged,
   and a view rooted at the child lags until its `main` catches up.

**What the Worker checks, and what it still trusts.** Nothing about a pin
is the client's word. The index the merge begins with is the only one it may
finish with, and before writing it the Worker has the container recompute
the index from root's pushed commit — every gitlink, recursively — and
refuses one that differs. The container expands only pins found in each
tracker's own repo, so a pin nobody pushed is never served. What the Worker
still takes on faith: that its container's answers are Git's (it runs the
image the Worker deploys), and, for merges into a branch other than `main`,
nothing more — a child's commit there lands on the child's branch of that
name, by the child's maintainers when it is not forward along `main`.

**`--only` is dropped (decided).** Merge stacking by tracker (`newgit merge
<instance> --only supabase`, once planned) is not coming. With the root as
spine, moving the root's `main` to an instance's commits moves every child
those commits touched. You can't merge `supabase`'s half of a commit and
leave `electron`'s, any more than in a Git monorepo: a partial merge would
be a cherry-pick, with new ids. Work that should merge separately goes in
separate instances. `requires` stays for binding (below); checking it
between instances (`spawn --from <instance>`) is planned.

**Pulling a child is explicit (decided).** A child's `main` moves when an
instance rooted at it merges, when `newgit pull --remote` brings in a PR
merged on GitHub (#142), or when an upstream is synced. The parent
sees it when someone runs:

```sh
newgit pull electron
```

That makes one view commit, `Update trackers: electron (14 commits)`, which
moves the pin and holds the child's files there, on one parent. It is authored
by whoever ran it, needs the root's maintainers like any change, and merges
into `main` like any instance commit; the workspace fast-forwards to it.
`follow` names the ref it pulls. Spawn never writes a commit, but it says
when a mounted tracker is behind its `follow` ref.

`newgit pull` with no tracker (#345) is the same for every tracker in Git
mounted here, at any depth, in one commit: `Update trackers: t3code (14
commits), ui-kit (2 commits)`. `newgit pull t3code ui-kit` does it for
those two only (#426): every argument is a tracker, and `--instance <name>`
names an instance other than the one it runs in. A pin is part of a
commit, so it lands on the branch it ran on, as `flake.lock` does: an
instance's branch in an instance, and in the project root the project
root's own instance branch, which `newgit merge main` publishes; `main`
takes it only when that merges. It writes nothing when no pin is behind.

**Update means upstream too (decided, #426).** For a tracker with an export
target rooted at it (t3code → Spheroman/t3code), "update" means what is new
*there*, as `nix flake update` would have it. So before moving pins, `newgit
pull` does for each such tracker what `newgit pull --remote <target>` does
to its `main`: fetches the target's branch, then fast-forwards the
tracker's `follow` ref to it, or really merges the two (one merge commit,
`Merge github.com/Spheroman/t3code newgit`, on that ref and the target's
tip), and the pin moves to that. That is one more fetch per such tracker,
on top of the one per tracker that resolves its `follow` ref with a remote.
What it does *not* do is move the tracker's `main`, anywhere: `main` is
shared, and landing commits there would change what every other instance
pulls next. The target's commits ride on the tracker's branch for this
instance (`instances/<slug>`) like any instance commit, the pin moves on
this branch only, and `newgit merge` lands them on the tracker's `main`
when the instance merges. That holds in the project root too: there, `newgit
merge main` lands them. `newgit pull --remote` is unchanged, and still the
one command that moves a tracker's `main` straight from a target.

**A child moves in the tracker that mounts it (decided, #426).** A child's
pin is part of its parent's commits, so moving it is a commit in the
parent, whose own pin then moves in *its* parent, up to root: one commit per
tracker whose pins moved, made bottom-up, each naming what moved below it
from where it stands. Pulling `t3code/repos` makes `Update trackers: repos
(1 commit)` in `t3code`, on t3code's pin, and root's view commit `Update
trackers: t3code/repos (1 commit)` pins that. `newgit pull t3code/repos`
does the same for that child alone; naming `t3code` moves only t3code's own
pin. A child whose parent this instance does not bind stays: an instance
commits only to trackers it binds (`newgit tracker use <parent>` binds it).
It is named when it was asked for, or when its `follow` ref is ahead of its
pin, so an instance that dropped the parent is not told about it on every
pull. A child named beside a parent whose target fails is listed as
skipped with it.

**The merge with a target is the same commit everywhere (decided, #426
review).** It is newgit's, made on nobody's behalf: author and committer
`newgit <newgit@localhost>` (the identity of an endpoint's `Merge main from
github`), not whoever merged the pull request on GitHub, dated as the parent
committed last (the target's tip on a tie), with the same message, and never
signed. Two instances pulling the same two diverged tips make one commit,
so merging both lands one merge on the tracker's `main`, not three.
Unsigned, it is rejected by an export target that requires signed commits:
an export carrying it fails there
([#443](https://github.com/Spheroman/newgit/issues/443)).

A force-pushed target is merged with the line the tracker already has, as
`git pull` would: the commits the force-push removed stay in the tracker's
history, and the next export pushes them to the target again. Reverting the
change in an instance and merging it keeps it out of the tip only: the
removed commits are still in the history, and the next export still
publishes them. When the force-push was to purge a secret, that publishes
it again, and nothing in newgit can purge it yet
([#442](https://github.com/Spheroman/newgit/issues/442): a way to reset a
tracker's line to its target). Until then, do not export that tracker.

Five kinds of tracker stay put, and it says which: a pin holding commits
its `follow` ref lacks (the instance's own work on it; moving the pin would
drop them, so merge first; such a tracker is listed as diverged and does
not take its target either, until `newgit merge`), a tracker the remote will not let the caller
read (a 403 on its repo; plain `newgit pull` fails when it can read none of
them, since then nothing is known to be up to date), a tracker whose export
target cannot be pulled from (unreachable, a real conflict with its
`follow` ref, a file of a tracker outside Git): it and every tracker below
it stay, never half-moved, since the target ships them too, a child of a
tracker this instance does not bind, and a tracker outside Git, which has
no pin (`newgit tracker pull` updates its files, and can overwrite an
instance's). The rest still move. A tracker named on the command line that
stays for any of these reasons makes it exit non-zero, after the others
moved.

Parallel instances merge the way a monorepo does: two instances that each
made a commit produce a merge commit in `main`'s view, even if they touched
different trackers. That is what Git would show.

**`requires` governs binding (built).** An instance cannot bind `electron`
without `supabase` (or, for a role, some tracker filling it): `newgit
tracker use electron` brings in what it requires, and dropping a tracker
others require is refused, naming them. Trackers have `requires` (trackers
or roles); resources have `depends_on` (resources or trackers; roles
planned) — one model.

Env-file trackers move by `newgit tracker merge <tracker>`, which promotes
an instance's revision to the tracker's `main`. Today `merge_with_source`
decides whether a source merge carries a tracker too; the planned `merge`
key would replace it, and `explicit` trackers would never be carried by
`newgit merge`.

---

## Rooting at any tracker

**Spawning at a tracker** (`newgit spawn --root electron`, planned, step 7)
composes the view rooted there. Resources come from the trackers in that
view. A tracker that mounts nothing in Git composes to its own history
unchanged. One that does composes like any root (its pin-only commits
rebuilt with their children's metadata), so its view is no longer its raw
history. That is the view a target declared by that tracker ships, with the
children that inherit its export (*GitHub is an export*).

**A root that `requires` its parent** (rooting at `electron`, which needs the
workspace `package.json`) is an error naming the parent to root at instead
**(decided)**. Rooting at the parent automatically would quietly give you a
repository you didn't ask for.

**A tracker cannot be carved around another yet (decided 2026-11-06).** Carved
around `lib`, `a` would not hold it: `lib` stays its old parent's, and a view
rooted at `a` would lack `apps/a/lib`. Making it a child is a move, which changes
its id, so `tracker create` refuses and names it. An export rooted at a tracker
refuses too if a tracker from elsewhere sits inside it, which a definition
written by hand can still arrange: it never loses files without a word.

**Two trackers of one name are two trackers (decided 2026-11-06, replacing
"the same tracker mounted twice").** A name is local to its parent, so a `ui`
in root and a `ui` in `app` are different trackers with different histories,
as two directories named `ui` are different directories. An edit to both
splits into both. Sharing one history by reusing its name, which is how
`ui-kit@v2` here and `ui-kit@v1` there used to work, is gone. That capability
comes back on purpose as a mount that names what it mounts: a shared tracker
as a link to one place at one version, and a dependency as a remote tracker at
a version pinned by the parent (#201, #202). Neither is built.

**Layering** (a lower tracker supplies defaults, the upper takes the writes)
is not built **(decided)**. Separate file names (`.env` and `.env.local`)
cover every case found so far, and layering needs whiteouts.

## Users, remotes, and trust

None of this is needed for the engine. These are positions, so it doesn't
paint them into a corner:

- **A profile is a root that mounts your projects,** plus an *overlay*
  tracker, bound into instances, that only fills paths other trackers
  exclude. Keeping them as two trackers avoids the newgit ⊃ jack ⊃ newgit
  cycle without a special binding rule.
- **Cost of composing a profile:** each tracker's view is composed once per
  compose, so a profile costs the sum of its projects once, plus the
  profile's own commits. To be measured with t3code as part of #144.
- **Code enforces, trackers hold policy.** Reading policy from `access`'s
  `main` is the first instance. Per-remote trackers, home trackers for
  branch heads, and per-audience registries stay as the #201 comments
  propose. They are a remote redesign, later. The root's ref as the commit
  point already answers #201 item 3 for one root.
- **Remote naming** follows a tracker's home with that redesign **(decided
  2026-11-06, replacing a flat `<owner>/<tracker>`)**: `spheroman/newgit`,
  `spheroman/newgit/t3code`, `spheroman/newgit/t3code/repos`. A tracker has
  one home and so one name, and any number of mounts; mounting never changes
  the name, and moving it to another home is a rename, with a redirect.
  `Spheroman/t3code` on GitHub is an export target, nested no deeper only
  because GitHub cannot. Today a name is `<owner>/<project>`.
- **A fork keeps its upstream's audience as a ceiling** (#227), unchanged.

**#202.** The four decisions this design leaves room for: mounts accept a
ref (pins hold exact commits and `follow` names the ref, so `ui-kit@v2` is a
`follow` value; "pin by forking" is no longer needed, since every parent
commit is already a lockfile); resources fill excluded paths (ownership rule
4); content-keyed resource outputs and resources declaring permissions are
independent of compose.

---

## On the remote

**A branch is root's ref, with an index (decided).** A newgit branch is a
ref on the root tracker's repository; `main` is the default branch, as on
GitHub. ProjectDO's list for the branch — one commit per tracker in Git, the
root's and every pin's, recursively — is an index written after the ref
moves, so that spawning, `clone`, and the Git endpoint know which repos to
fetch without expanding pins first. A checkpoint is a frozen list and an
instance is a list being worked on, so spawning starts an instance from a
branch's list. `newgit remote add` creates `main` from the project root's
current state, its first publication; after that `main` moves only by
`newgit merge`. The remote lists branches, creates one from a list, and sets
the default (`/branches` routes in `remote/src/index.ts`); the CLI to match,
`newgit branch create <name> [--from <instance>]` and `newgit branch default
<name>`, is planned. (A newgit branch is not the Git branches an agent makes
inside one workspace.)

**A tag is a frozen list (decided 2026-11-08).** A tag names one point of
the project's history: a root commit on root's `main`, frozen with the index
that commit pins, as a checkpoint is a frozen list. ProjectDO keeps them
(`/tags` routes); nothing is written to root's repository, because a ref
there would name a root commit, which no reader sees. The Git endpoint
serves each one as `refs/tags/<name>`, composed for the reader as a branch
is, so `git fetch` brings it at the commit that reader's `main` has. An
annotated tag keeps its tagger and message, and each view's tag object is
made again from them at that view's commit, the same object every time; a
signature covers one object, so it is not carried. A tag never moves: the
same tag again is a retry, another commit or annotation is refused, and
deleting one is the remote's `DELETE /tags/<name>`, by whoever may make it.
Whoever may merge into root makes and deletes them, from a login, not an
agent token. A tag comes from `git push origin <tag>` in a linked project
root, which then pushes it to each export target composed as that target
sees it, or from a tag made on a mirror (*Coming back*). An instance's push
carries no tags on a newgit remote, so one there is refused, saying to merge
and tag from the project root.

| Repo (Artifacts) | Holds | Read | Write |
|---|---|---|---|
| `<owner>.<project>.t.<tracker>` | the history of a tracker in Git, at its own root | its audience | seeding, then merges |
| `<owner>.<project>.i.<instance>.<tracker>` | an instance's branch of one tracker in Git, forked the first time the instance changes it | the tracker's audience; only the instance's owner when it is private | the instance |
| `<owner>.<project>.t.<tracker>[.<login>]` | a tracker outside Git, as before: age ciphertext and a `.newgit/recipients` record unless `public` (#338) | its audience | its audience |

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
tracker repos, composes the branch from root's ref, expanding only the pins
*this caller* may read and that are found in each tracker's own repo, and
answers with `git upload-pack`. A workspace's `origin` fetches from it, so
`git pull` shows `main`. Each person's view holds only what they may read,
enforced by the server; nothing is pre-built, so nothing is rebuilt on
merge. When the sandbox goes idle, an alarm snapshots its tracker repos and
composed views (if anything was composed since the last snapshot) and then
stops it, so a cold start restores rather than re-clones. A snapshot is the
whole container, the `newgit` binary included, so one taken on an earlier
image is dropped and the next start is from the image: a deploy reaches
every project, at the cost of one re-clone each. Read-only: pushes go
through a newgit workspace, which splits locally.

Why a sandbox and not the Worker: a Worker cannot run Git, and composing in
JavaScript would mean writing Git's object model ourselves.

**Who may change what (decided).** A tracker's `maintainers` may merge
changes to it; by default they are its audience — anyone who may read a
tracker may change it. Who may change a tracker's *security* (its paths,
audience, maintainers) is then just who may change the tracker that owns its
definition file. That is `access` — which `newgit init` creates with
`maintainers = "owner"`, mounted at `.newgit/trackers` claiming `.` — unless
a tracker in Git mounted beside it at `.newgit/trackers` claims the file
(ownership rule 1: trackers sharing a mount split by their claims). So governance layers with no special case: project
trackers; an admin tracker mounted at `.newgit/trackers` claiming the
definitions it governs (readable by everyone who uses them, since newgit
needs their paths, but maintained by admins); and a tracker of yours
claiming the admin tracker's. Everything else in `.newgit/` — resources,
scripts, config, exports — is root's, which anyone who merges may change.
The remote reads each definition from the tracker that owns its file, at
that tracker's own `main` (whatever the project's default branch is called),
to a fixed point.

**Governance only delegates downward (decided, #228).** Ownership alone
would let a tracker handed one definition claim any other, `access`'s
included, and with it the whole project: the claim is a change to the
claimant's definition, which only the claimant's governors approve, not
whoever governs the claimed file. So a claim on `<T>.toml` counts only when
the claiming tracker's own definition is held by the tracker that holds
`<T>.toml` — then the claim is written in that holder's repo, and the people
who govern `T` are the ones who approved it. A tracker keeps what it holds
while it still claims it; `access.toml` is always `access`'s; no tracker
holds its own definition; and no claim closes a loop, so every chain of
holders ends at `access`. A claim that doesn't count is ignored by the
remote (the file stays with its holder). The client refuses the two it can
see in a single definition, a claim on `access.toml` or on the tracker's own.
*(Not enforced for trackers outside Git yet, and open: the remote gives a
write credential for the repo of a tracker outside Git to anyone in its
audience, and that credential can push the repo's `main` directly. For
`site`, that is the page newgit.dev serves. Credentials are scoped to a
repo, not a ref, so enforcing `maintainers` there means the merge of a
tracker outside Git has to go through the Worker, as `newgit merge` does for
a tracker in Git.)*

**One name for a commit, whoever reads it (decided).** A Git commit id is the
hash of the commit's contents and its parent's id, so someone whose view
leaves out a tracker sees different ids from the first commit that touched
it onward. Two layers keep that from mattering:

- **Usually it never happens.** Trackers in Git are readable by every member
  by default, and everyone who reads the same code gets the same commits.
  A tracker in Git with a narrower audience should need an explicit flag,
  whose warning names both costs: its past stays readable in root's
  history, and people who cannot read it see different commit ids.
  *(Planned: today `tracker create --git --audience` takes any audience
  without a warning. What is built is the split's refusal of one commit
  spanning audiences, under* One commit, one audience*.)*
- **When it does, every commit has a canonical id:** the commit as its author
  made it, which the root commit split from it carries as `Newgit-Split:
  <id>`. A commit given back as its original (*Signed commits*, rule 2) is
  its canonical id. The endpoint notes it on each commit that stays rebuilt,
  in `refs/notes/newgit-canonical`, served to Git clients as
  `refs/notes/commits` so `git log` shows it; newgit commands accept either
  id, and newgit's own surfaces — checkpoints, branch lists, the web UI —
  always show canonical ids.

## Git and GitHub

**A GitHub repo holds only its own trackers (decided 2026-11-05).**
Spheroman/newgit holds newgit's, Spheroman/t3code holds t3code's.

**GitHub is an export (decided).** `newgit export --remote <name>` pushes a
composed projection of the trackers' `main`s, with composed history, to an
ordinary repository — GitHub's, say — that a tracker's own definition
declares **(decided 2026-11-08, replacing `.newgit/exports/<name>.toml`)**:

```toml
# t3code.toml
export = { url = "github.com/Spheroman/t3code", branch = "newgit", branches = true }
# t3code's repos.toml
export = "inherit"       # ships inside t3code's export, wherever that goes
# a secrets tracker: no export key, so it never leaves newgit
```

- **The declaring tracker is the target's root.** Declared by `root`, it is
  the project's own view; declared by another, it is the view rooted at it,
  its files at the top, which for a tracker that mounts nothing in Git is its
  history unchanged (#141).
- **No key means no export,** which fails closed. **`"inherit"`** takes the
  parent's export, whatever it is: one place, several, or none. The parent
  is the tracker defining this one (`t3code` for `t3code/repos`, root for
  what `access` defines). **A list** covers a tracker inside the monorepo's
  mirror and on its own: `export = ["inherit", "github.com/x/lib"]`.
- **Carving writes `"inherit"`.** `newgit tracker create --git --path lib`
  does, so carving never silently drops a directory from a mirror. The
  key's default stays nothing; the command writes the obvious line, unless
  the new tracker's audience is narrower than its parent's (and not
  `public`), when it writes none and says so.
- **A target's options live where its URL is declared.** `branch`,
  `branches`, and `name` belong to the declaration; a tracker that inherits
  shares that one target, as it shares its one composed history and one
  branch, so it has no options of its own. A target is named after its
  repository (`t3code`), or its `name`; that is its remote's name in a
  workspace and what `--remote` takes. One repository is one target: two
  declarations of it are refused.
- **Inheriting never passes an audience.** A tracker that inherits a target
  but whose audience is narrower than the declaring tracker's (and not
  `public`) refuses the export, naming it. A view rooted at a tracker that
  lacks a tracker mounted inside it but defined elsewhere is refused too,
  rather than losing its files without a word.
- **Exports are owner-governed.** Definitions live in `access`, so widening
  where something ships takes an owner's merge, where `exports/` was
  ordinary root content.
- **Tags go only where root's history goes.** A project's tag is pushed to
  the target root declares, which every inheriting tracker ships inside,
  never to one declared by a child: that is the child's own history, with
  versions of its own (t3code's `v0.1.x`), and a tag never moves, so the
  project's `v0.9.0` would take that name from it for good. Tags coming in
  from such a target are refused for the same reason.
- **What this gives up:** a child inherits all of its parent's targets, so
  two targets declared by one tracker show the same trackers. A view that
  differs is declared by a different tracker.

A tracker left out leaves no path, no pin, and no history there, and a
commit that only moved its pin is not shown, so the target's ids differ from
the view's from the first commit that touched it. GitHub never holds a
tracker the export did not include, so `.env` files can live in the project
and never reach a public mirror.

**Coming back (decided 2026-11-05).** Ids differ on a mirror, so what is
merged there comes back through newgit, never by fetching GitHub. A linked
project root's `origin` is the Git endpoint, as a workspace's is, so `git
pull` serves the project's own commits, and a commit on the mirror becomes a
root commit with the withheld trackers' pins carried over: by hand with
`newgit pull --remote <target>`, or by a webhook
([remote.md](remote.md), *The GitHub webhook*). Such a root commit records
the mirror's commit in its trailers, headers included, so composing for that
target gives that very commit back, GitHub's signature and all, and the next
export is a no-op. For a reader of everything it is a rebuilt commit with a
canonical note, like any other (*One name for a commit*). A tag made on the
mirror comes back the same way, through the webhook: its commit is in `main`
composed as the target sees it, under the very id it has there, so the
remote knows which root commit it names and makes the project's tag there
(*A tag is a frozen list*).
 Since `.newgit/` is root's, exporting root ships the config,
resources, and scripts; only `.newgit/trackers/`, where every `export` is
declared, follows `access`'s audience.

**Adopting an existing repository (decided).** `newgit init` in a Git
repository imports its history as `root`'s history, up to the first commit
that mounts a tracker in Git (a repository that already carries newgit's
definitions); commits past that are view commits, and split makes root
commits of them. A fresh project has no such commit, so all of it is
adopted, and every view id is unchanged by construction.

---

## Migration

Before roots, a project's trackers in Git were `root`, `project` (owning
`.newgit/`), and `access` (owning `.newgit/trackers/`, unmounted), and any
others were carved out of root's history by path and composed back by date.
`newgit tracker migrate` moves such a project onto pins. Since carving is a
commit, no view history is rewritten **(decided)**:

1. `project`, `access`, and every other tracker in Git fold back into root.
   Root's history becomes the view's history, commit for commit: a commit
   from before mounts pins nothing, so it composes to itself, and every view
   id is unchanged by construction.
2. One commit at the tip of `main` mounts them again: `access` at
   `.newgit/trackers` with `follow = "main"`, each other tracker in Git at
   the path it owned, and `project` is gone. Splitting it gives root its
   first pins.
3. **Gate:** every branch and checkpoint is recomposed in a fresh repository
   holding only the new tracker commits (the remote's situation), and
   nothing moves unless each comes out with the id it has here, signatures
   included. Everything from before the carve pins nothing and is its own
   root commit, so it reproduces by construction, GitHub's signatures with
   it; what proves the engine is what is rebuilt from a split. So the gate
   also splits a test commit on top of the carve, changing a file of root's
   and one of `access`'s and made the way you make commits (signed, if you
   sign), and recomposes it there. Its message says how many views it
   rebuilt and how many it reused.

Nothing old is deleted: the store's old tracker refs move under
`refs/newgit-premigration/`, and the project repo's old notes with them. On
the remote it moves every branch: `main` to the carve's index, each other
branch to the root commit its old list was (a branch whose old list names no
view this machine split is refused, by name, before anything is pushed),
pushing each tracker repository on what it holds now. Jack is the only user,
so no compatibility path is kept; `--dry-run` says what would move.

It resumes. The carve and its split are made once and kept under
`refs/newgit-migrating/` until the end, so a run stopped part way (the
remote failing as the merge finishes, a checkpoint write dropping, the local
ref transaction failing after the remote moved) is finished by running it
again, with the same commits: a list already at its new index is left alone,
a ref already at its target is not pushed, a checkpoint already naming root
commits is not rewritten, and the local steps after the store moves repeat
safely. A checkpoint this login may not rewrite (someone else's instance) is
named and skipped.

## The t3code milestone on this design

- **#139 (compose an imported tracker):** replaced by this design. #151 is
  closed unmerged; its tests carry over as acceptance cases (the join keeps
  ids, old branches don't get t3code until they merge `main`, upstream syncs
  go on top whatever their dates, log and blame through the join, non-UTF-8
  headers), plus the cases its last review found: a pre-import branch merged
  into `main`, and a repair over a join.
- **#140 (import):** *done.* `newgit tracker create t3code --git --mount
  apps/t3code --from <url> [--from-branch newgit]` fetches the branch
  unchanged as the tracker's `main`, then makes one commit in the project
  root adding the definition (in `access`), the mount, and the pin (a
  commit on one parent). Refused, leaving nothing behind, when the mount has
  files or the history adds a file a tracker outside Git owns there.
- **#141 (export a tracker as its own root):** *done.* A target declared in
  `t3code`'s own definition pushes t3code's own `main`, composed rooted at
  it. While
  t3code mounts nothing in Git, that is its own history, so the first export
  is a no-op fast-forward of Spheroman/t3code, ids and signatures intact.
  Once it mounts one, commits from then on are the view's, with the child's
  files in place and ids of their own.
- **#142 (pull from a target):** *done.* `newgit pull --remote t3code`
  fetches, fast-forwards or merges t3code's `main`, then moves its pin in
  the project root as `newgit pull t3code` would (plain `newgit pull` does
  the same fetch first, landing on the branch it runs on, #426). When t3code mounts
  trackers, what comes back is view commits, so it is split first: each file
  goes to the tracker that owns it, an upstream's plain commits and a merge
  of them included. For Spheroman/newgit, its
  commits are view commits: ours are recognised by id, by the note on each
  exported commit, or by the canonical notes, and the rest come into the
  project root as ordinary work, split when `newgit merge main` publishes.
- **#143 (`gh` per target):** done: a push resolves `gh` to the target that
  ships a tracker the work touched.
- **#144 (bring t3code in):** Spheroman/newgit's export *leaves t3code out*
  (decided 2026-11-05, replacing 2026-11-04's): Spheroman/t3code is t3code's
  mirror, rooted at it, and importing t3code once put 8,164 of its commits
  into newgit's view and onto the public mirror. FORK.md: upstream comes in
  by merging pingdotgg/t3code into the fork's `newgit` branch, then `newgit
  pull t3code` (or plain `newgit pull`), one commit here. The
  cold-start measurement doubles as the profile cost check.

---

## Build order

Status as of 2026-11-11. Each step lands alone and keeps the suite green.

**Before roots (done, 2026-10):** tracker repos with real history (`main`,
`instances/<instance>`) in the shared store; the `root` tracker, ownership by
specificity, and the `git` flag; the projection (compose/split engine,
trackers in Git, wiring into spawn, push, and checkpoint sync, the remote
with one Artifacts repo per tracker and the Git endpoint); roles, priority,
`requires`, resources, `use`/`drop`; `newgit merge [instance] [--into
<branch>]` moving every tracker together; the GitHub export
(`branches = true`; its targets are now declared by `export` in the
definitions). The engine of that
period — interleaving by committer date, `Newgit-Split-Parent`, carving with
history, the `project` tracker, `tracker repair` for commits split before
headers were recorded — is replaced by the steps below and deleted.

1. **`secret` → `git`, alone.** *Done* (#320). It touched every definition
   and fixture, so it went first (AGENTS.md: a shared rename goes first).
2. **Compose and split on pins.** *Done* (#201). The compose and split
   above, `newgit merge` at root's ref, `newgit pull <tracker>`, `mount`,
   `follow`, and `!` claims, carving as a commit, the same tracker mounted
   twice, `project` folded into root for `newgit init`, and the migration's
   gate as the test everything else waited on.
3. **Migration.** *Done:* `newgit tracker migrate`, run locally, then on
   newgit.dev.
4. **Delete the old engine.** *Done:* date interleave, group scheduler,
   `Newgit-Split-Parent`, carving with history, `tracker repair`,
   workspace-path storage, `project`.
5. **The t3code milestone** on the new engine: #140, #141, #142, and #143
   are done. For #144, a mounted tracker joins as one commit, a mirror holds
   only what it ships, the project root fetches from the endpoint, and a
   webhook brings GitHub's merges in; bringing the real t3code in is next.
6. **Roles are forks** (#201's *Roles are forks* build order): step 1,
   `tracker create --from` for trackers outside Git, is *done* (#415; see
   *Binding*). Merging between members with `--disclose` is next.
7. **Later:** spawning at a tracker (`spawn --root`), `.gitignore` as
   definition, moving `role`/`priority`/`requires` to the mounting parent,
   `spawn --from <instance>` with `requires` checked between instances,
   profiles, the per-remote redesign, the `newgit branch` CLI.

Then the preview work in [remote.md](remote.md) continues on top: a
preview's environment identity is simply the set of tracker commits it
composes.

---

## Decided with Jack (2026-10-04)

The rest of that day's decisions — trackers outside Git out of the
workspace's Git, splitting when work leaves the machine — are recorded where
they apply above. Two of them were revisited on 2026-11-04: *full replay*
(interleaving every tracker's history by date) became *the root as spine*,
and *merges stacking by tracker* (`--only`) was dropped; see below. Two add
something:

1. **The project root is a workspace:** the instance `main`, bound to every
   tracker's `main`. You work there as today; its commits reach a shared
   tracker's `main` only through `newgit merge main`.
2. **The video leads with spawning anything, anywhere** — an agent's attempt
   on your machine or as a preview, with its whole environment and your
   keys — with trackers shown as how it works.

## Decided with Jack (2026-11-05)

1. **A GitHub repo holds only its own trackers,** and a mounted tracker's
   history stays in its own repo and never enters the mounting project's Git
   history. This replaces 2026-11-04's *Spheroman/newgit mirrors t3code too*
   and the replay rule: a mount or a pin move is one ordinary view commit.
   The cost, accepted: `git log`, `git blame`, and `git bisect` under a mount
   stop at the import and at each pull. The child's own history answers
   those.
2. **A project's `origin` is its remote's endpoint.** A mirror's ids are not
   the project's once it withholds a tracker, so the project root fetches
   from newgit.
3. **A mirror's merged commits come back by webhook,** as root commits, on
   the same terms as `newgit pull --remote`.

### Settled while building (2026-11-05)

- **`trackers` on a target is exact.** It used to add to the `public`
  trackers, so a public tracker (t3code) could not be left out. (Since
  replaced by `export` in the definitions, where no key ships nothing.)
- **A root commit brought in from a mirror names the mirror's commit,** not
  the view commit rebuilt from it, so its id survives the round trip.
- **The project root learns what `git pull` brought.** The view commit of
  such a root commit is not what its trailers name, so the first sync after a
  pull composes the remote's `main` here, which notes it. A workspace that
  `git pull`s `origin` instead of `newgit pull` does not, and its split makes
  a commit of its own, which merges.
- **A mirror's event lands as a merge by whoever pushed it:** they must be a
  member and maintain what it changes (*The GitHub webhook*).
- **A mirror's commit that holds withheld work is refused.** A pull
  request's branch leaves out what the target withholds, so landing it would
  lose an instance's edit there. Only an instance GitHub names as the pull
  request's branch counts; when GitHub cannot say, the pull waits for it
  rather than refuse. The merge goes through `newgit merge <instance>`
  instead, and a mirror's merge then takes each withheld tracker's files from
  whichever parent is ahead, not its first ([remote.md](remote.md), *The
  GitHub webhook*).
- **`git push` in a linked project root publishes.** Fetch stays on the
  endpoint; a push goes through the outbox and `push-receive-pack`, as a
  workspace's does. Pushing the project root's branch is `newgit merge main`,
  then an export to each target whose view of `main` is not what was last
  pushed there, so one push of root and mount work updates both mirrors, each
  with only its own trackers. A refused merge fails the push and nothing is
  exported. A merge stays once it has landed: a failed export fails the push
  and names the `newgit export --remote <target>` to rerun. A push does not
  pull, so a target that is ahead is such a failure. Any other branch is
  refused, saying to spawn an instance. Pushing straight to a `branches =
  true` target is refused, as in a workspace. A tag pushed from there (after
  the branch, when both are) is made on the remote, then on each target; one
  naming a commit `main` does not have yet is refused, saying to push `main`
  with it (*A tag is a frozen list*). The outbox holds no tags, so a tag
  pushed again after a target refused it reaches newgit, not `up-to-date`.

## Decided with Jack (2026-11-04)

1. **The parent records the child's commit** (gitlink pins under
   `.newgit/pins/`), and the root's history is the view's shape: one root
   commit per view commit. This revisits 2026-10-04's "full replay". The
   consequence: parallel instances produce merge commits in `main`, as in a
   monorepo.
2. **One kind of tracker.** `secret` becomes `git`, which says only whether
   the tracker's content is committed with Git. Sensitivity is `audience`
   alone. The default stays `git = false`, so a forgotten flag can't leak.
3. **Shape from the commit, policy from `access`'s `main`, claims only in
   split.** Mounts can move (a commit moves them); editing a claim never
   rewrites history; nobody rolls policy back with a pin.
4. **The message stays with the work:** on the root when it changed the
   root's files or nothing else holds the work, on the child commits
   otherwise. Any mixed audience among commits that carry the message is
   refused.
5. **Carving is a commit,** and so the migration rewrites no view.
6. **Pulling a child is explicit** (`newgit pull <tracker>`); nothing writes
   commits on anyone's behalf. `--only` is dropped.
7. **A merge commits at the root's ref,** with pin moves checked by the
   Worker before the push.
8. **`project` folds into the root tracker.**
9. **Layering is not built.** Rooting at a child that requires its parent is
   refused.
10. **Spheroman/newgit mirrors t3code too.** Replaced on 2026-11-05: it does
    not.

### Settled while building (2026-11-04)

Where the code ended up differing from the design as written that morning,
resolved while building steps 2–4. Each is recorded above where it applies;
this is the list.

- **A tracker's children are defined in its own `.newgit/trackers/`.**
  Root's live in `access`, mounted there; before `access` is carved, in
  root's own tree. A mount is relative to the parent's root, and nested
  mounts work through this. The design said only "`access` owns the
  definitions".
- **Two exemptions from one-audience.** A merge is exempt only when its
  message is one nobody wrote: Git's own one-line form or newgit's. So is
  the carve commit newgit makes,
  `Create tracker `x` at <path>`: without that, carving a narrow-audience
  tracker out of root would always be refused.
- **An index lists one commit per tracker name.** With a tracker mounted
  twice, only the first copy is listed. The second copy's commits are in the
  store locally but on no branch list yet.
- **Commits from before mounts are their own root commits.** A view commit
  with a legacy layout (`project` holding `.newgit/`, `access` unmounted, or
  no mounted tracker in Git defined) pins nothing and is its own root
  commit. A commit whose definitions mount a tracker in Git it does not pin
  is a view commit that only a split can make a root commit of. `newgit
  init` therefore adopts root's history up to the first commit that mounts a
  tracker in Git.
- **Trackers sharing a mount split by their claims** (longest wins). So
  governance layers by mounting a tracker beside `access` at
  `.newgit/trackers`, claiming the definition files it governs; `tracker
  create --git --path .newgit/trackers/x.toml` is no longer the way. Carving
  inside another tracker's mount makes a child of that tracker (*Who owns a
  path*).
- **Trackers outside Git may have a `mount`.** They are never pinned.
- **Split refuses more than the design listed:** unowned paths (every claim
  excludes them) and paths owned by a tracker outside Git, besides unbound
  trackers, `.newgit/pins/`, and mixed audience.
- **Exporting root ships `.newgit/`** (config, resources, scripts, exports),
  because `project` folded into root. Only `.newgit/trackers/` follows
  `access`'s audience.
- **The Worker checks pins with Git.** It cannot run Git itself, so the
  endpoint's container answers two questions it asks: before a merge, how
  each child's pin moves along that child's history (forward along `main`
  needs root's maintainers; back, sideways, or off `main` needs the
  child's), and before an index is written, whether root's pushed commit
  pins exactly that. Nothing about pins is the client's word.
- **A merge into a branch other than `main`** keeps a child's new commit on
  the child's branch of that name; only a merge into `main` moves a child's
  `main`.
- **A pin its tracker's repo does not have** leaves that tracker out at that
  commit, as for a reader who may not read it, rather than failing the view.
- **During the move to mounts the Worker reads both layouts** of `access`
  (`<name>.toml`, and `.newgit/trackers/<name>.toml` from before), so a
  remote deployed before a project migrates still reads its definitions.
  The old read goes once every project on the remote has migrated.
- **The migration folds every tracker in Git,** not only `project` and
  `access`, and mounts each of the others again at the path it owned.
- **A rooted export ships the tracker's own `main`,** not the commit root's
  `main` pins: a branch is a ref of the root a view is rooted at, and
  `branches = true` pushes each instance's own branch of the tracker. On a
  linked project that `main` is the remote's, so a pull request pulled back
  goes out again only once `newgit merge main` has published it; until then
  the export says so, and a repeat `newgit pull --remote` finds nothing new.
  Unlinked, the store's `main` already has it.
- **An export finding a rooted target ahead refuses** and names `newgit pull
  --remote`, instead of pulling by itself as an export of the project does:
  bringing those commits in moves a pin, which is a commit nobody asked for.
- **`newgit pull --remote` lands in the project root only,** and publishes
  nothing; `newgit merge main` does. Instances pick a moved pin up with
  `newgit pull <tracker>`, which also takes what is new on the target
  itself, on the instance's branch, without moving the tracker's `main`. For a target without `root`, the files of each
  tracker it withholds are put back from the parent commit, and a commit
  touching a withheld tracker's mount is refused rather than guessed at.
- **The import's commit is unsigned,** since newgit writes it and its
  `access` half byte for byte so they compose inline. A pull's commit, and
  the merge `pull --remote` makes, go through `git commit-tree`, which
  honours `commit.gpgSign`: they are signed when you sign.
- **A tracker's branch on the remote falls back to the store only when the
  remote says it has none** (a 404 for the repo, or no such branch). Any
  other failure fails the command, so nothing is decided on stale refs.
- **Commits record where history came from without secrets or paths:** a
  URL loses its userinfo and query, and a local path is written as its
  `origin` URL, or only its directory's name.

---

## Links: one tracker in two roots

Built 2026-11-09 as #201's suggested experiment, after anchoring's step 8.
A tracker defined with `link = "spheroman/jack-env/dots"` is that project's
`dots`, mounted here. `newgit tracker link <name> <home> --mount <path>`
writes the definition and joins the home's `main` as one commit on the
project root; `newgit merge main` publishes it.

- **One history.** The link has no repo here. Its repo is its home's
  (`spheroman.jack-env.t.dots`), and its heads are in the home's
  `TrackerDO`. Each root that links it pins its own commit of it, in its own
  `.newgit/pins/`.
- **One `main`, every root told.** The home's object keeps a `linkers`
  table. A move of the home's `main` goes into its outbox for its own
  parent and for each linker's parent. Each root then rolls up as it would
  for a child of its own. A change merged from any root lands at the link
  (the anchor) and reaches every root.
- **Policy is the home's.** Reading and merging it is judged by the home's
  definition and by membership in the home project: linking needs read
  there, and moving its `main` needs its maintainers there. The remote
  records a link (`POST /trackers` with `link`) only for whoever may add a
  tracker here *and* may read the home. A linking root's credentials route
  answers for the home's repo, by the home's rules, and for no other
  project's.
- **Joining and leaving move nothing at home.** A merge that adds the link
  must name the home's head (otherwise it is stale: 409). Dropping the link
  changes only this root's pins. Deleting a linking root leaves the home as
  it was.

Checked live on the dev remote: a home and two roots linking its `dots`. A
change merged from an instance of one landed at the home, and both roots
rolled up. `git pull` from the second root's own Git endpoint brought
`Roll up jack-env to …` with the change in its `.jack-env`. A change from a
stale instance of the second root, to the same lines, was refused as a
conflict, and nothing moved.

What was clean:

- Nothing above the tracker objects changed shape. Fan-out was one more row
  per linker in the outbox, and the roll-up, the two-phase landing, and
  the endpoint's composition took a linked child like any other.
- Naming every repo through one per-request function (`repoNamer`, and
  `git_repo` in the client) was the whole of "where does this tracker
  live".

What was ugly, and is left that way for now:

- **Branches other than `main`.** The home's object holds a linking root's
  other branches as `<owner>/<name>:<branch>` (no Git branch has a `:`),
  told only to that root. It works, but an instance's change to a link
  lives in the home's object under a name the home never chose. `main` is
  assumed to be every project's default branch.
- **A change's own commit is not kept above a link.** `via` (fast-forwarding
  the parent to the change's own root commit) is dropped for a link, since
  that commit is one root's and the link has several parents. Each root
  gets a roll-up commit instead.
- **Repo `main`s follow by the anchor now.** Before, a child's repo branch
  moved only when the merging client asked with the right `old`. A link is
  moved from roots whose clients have different views, so its repo `main`
  stayed behind, the next root's client fetched a stale `main`, and a merge
  from it could move the home's `main` *sideways*, dropping the other
  root's change (seen live, then fixed). The landing's anchor now follows
  its head as root's branch always did. This is true of every tracker.
- **A home deleted under its linkers** is not handled. Their links fail
  to read.
- **No CLI to unlink** other than dropping the definition by hand.

Not found by this experiment, but found while running it: `newgit pull
<tracker>` in an instance replaced the tracker's files with its `main`
rather than merging the instance's own change into it, so the conflict
message's advice ("bring `main` into the instance's workspace, resolve
there") lost the instance's change if followed with `pull`. Since #426 a
pin holding the instance's own commits stays where it is, and `pull` says
so (and exits non-zero when the tracker was named), rather than dropping
them; bringing `main` in is still a merge, not a pull.

## Open questions and known gaps

Unbuilt parts of the design above are marked *planned* where they appear:
the `merge` and `production` keys, spawning an instance on another with
`requires` checked between them, spawning at a tracker, the `newgit branch`
CLI, the narrow-audience warning, `.gitignore` as definition, and
`depends_on` naming a role. Open issues on Spheroman/newgit:

- **Imported histories.** Import (#140), export as a root (#141), and pull
  back (#142) are built; bringing the real t3code in, and measuring its
  cold start, is #144.
- **A fork removed and made again under the same name.** `tracker remove`
  deletes this machine's record of what a fork started at, but `serve`'s
  record (`refs/newgit/from/<upstream>`) lives as long as the repo does, so
  a tracker re-added under that name (say `public`) is still capped to the
  old upstream's audience while the client believes it is what its
  definition says. It fails closed, but silently: nothing tells the client
  the server capped it. The server says so only when it refuses a read.
  Telling the client in its sync and merge output, naming the upstream on
  record, needs the server to hand the capped facts back on a write, which
  nothing does yet.
- **Per-user trackers.** A per-user tracker's `main` reaches the remote only
  as a side effect of some instance's checkpoint (#224).
- **Sparse checkouts.** Restoring a dirty checkpoint writes files into a
  dropped tracker's sparse-excluded paths (#225).
- **Governance.** A tracker handed one definition can claim `access`'s and
  lock the owner out (#228). No orgs: owners are always logins, and
  `project-devs` is the only group (#227).
- **Resources.** Nothing orders `start`, so a runtime dependency (functions
  needing supabase running) cannot be expressed (#54). Reviewing a change
  places each tracker it touched locally or in the preview; settled in
  [remote.md](remote.md), *Reviewing a change* (#229).
- **Forks on newgit.dev.** The Worker keeps no record of what a fork was
  made from and doesn't cap it, so there only the client holds the ceiling
  (#415). `newgit serve` does both (*Binding*).
- **The model itself.** The rest of #201: every tracker the same kind of
  thing, rooted at the user. The engine is built; what remains is step 7
  above, and the per-remote redesign under *Users, remotes, and trust*.
