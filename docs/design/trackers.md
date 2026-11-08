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
| `history` | How its parent records it rolling up (docs/design/anchoring.md): `commits` (the default), one commit per change so the parent's `git log` is complete; or `pins`, one snapshot of where it is now, carrying none of its messages. The parent's choice, as the mount is. Written only when `pins`. |
| `pinned` | Whether its parent holds it where it is (docs/design/anchoring.md, *Follow and pinned*): a change landing at it never rolls up into the parent, and moving the parent's pin is a change to the parent, approved by the parent's maintainers; rollback is a revert. For what must not move by itself: a private dependency, `/env/prod`. Default `false`. |
| `resources` | Resources this tracker brings. A resource is bound iff a bound tracker brings it. |
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
tracker you root at. Split assigns paths by this rule **(decided)**:

1. **The deepest mount wins.** A path under `apps/electron/` is decided among
   the trackers mounted at `apps/electron` or deeper. A parent's claim of `.`
   does not reach into a mount.
2. **Among trackers mounted at the same path, the longest claim wins.**
   `local-db` mounted at `.` claiming `.env.db` beats root's `.`. Trackers
   sharing a mount split by their claims: a tracker mounted beside `access`
   at `.newgit/trackers` and claiming `web.toml` owns that file; `access`
   keeps the rest.
3. **Identical claims at the same mount are an error, except within a role.**
4. **Claims can exclude**, `.gitignore`-style: `paths = [".", "!.env",
   "!node_modules/"]`. An excluded path is owned only by a tracker that
   explicitly claims it at that mount, or filled by a resource. Otherwise it
   is unowned, which is all "ignored" ever meant, and split refuses a commit
   that changes it.

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

**Landing is moving to anchors:** see [anchoring.md](anchoring.md), a draft.

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
planned (step 6). Compose doesn't depend on it.

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
| `newgit pull <tracker>` | Fast-forward: no history, `follow` lands on the pin and is counted. Otherwise `need_history`, so a pin is never moved backwards |
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

That makes one view commit, `Pull electron: 14 commits from main`, which
moves the pin and holds the child's files there, on one parent. It is authored
by whoever ran it, needs the root's maintainers like any change, and merges
into `main` like any instance commit; the workspace fast-forwards to it.
`follow` names the ref it pulls. Spawn never writes a commit, but it says
when a mounted tracker is behind its `follow` ref.

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

**Spawning at a tracker** (`newgit spawn --root electron`, planned, step 6)
composes the view rooted there. Resources come from the trackers in that
view. A tracker that mounts nothing in Git composes to its own history
unchanged. One that does composes like any root (its pin-only commits
rebuilt with their children's metadata), so its view is no longer its raw
history. That is the view an export target's `root` ships, children and all
(*GitHub is an export*).

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
| `<owner>.<project>.t.<tracker>[.<login>]` | a tracker outside Git, as before | its audience | its audience |

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
(ownership rule 2). So governance layers with no special case: project
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
composed projection of the trackers' `main`s, with composed history, to the
target defined in `.newgit/exports/<name>.toml` — an ordinary GitHub
repository, say. A target with `trackers` ships exactly those, of any
audience; one without ships the `public` ones. A tracker left out leaves no
path, no pin, and no history there, and a commit that only moved its pin is
not shown, so the target's ids differ from the view's from the first commit
that touched it. With `root = "<tracker>"` the target
gets the view rooted at that tracker instead: the tracker's own `main`, its
files at the top, which for a tracker that mounts nothing in Git is its
history unchanged (#141). What it mounts ships with it, since the repository
holds those files as its own, so carving a directory out of a tracker changes
nothing in its export but the one definition file. `root` names only the
tracker, so a child whose audience is narrower than the root's (and is not
`public`) refuses the export, naming it: leaving it out would lose files, and
sending it would pass its audience. GitHub never holds a tracker the export
did not include, so `.env` files can live in the project and never reach a
public mirror.

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
resources, scripts, and export targets; only `.newgit/trackers/` follows
`access`'s audience.

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
- **#141 (export a tracker as its own root):** *done.* `root = "t3code"` on
  an export target pushes t3code's own `main`, composed rooted at it. While
  t3code mounts nothing in Git, that is its own history, so the first export
  is a no-op fast-forward of Spheroman/t3code, ids and signatures intact.
  Once it mounts one, commits from then on are the view's, with the child's
  files in place and ids of their own.
- **#142 (pull from a target):** *done.* `newgit pull --remote t3code`
  fetches, fast-forwards or merges t3code's `main`, then moves its pin in
  the project root as `newgit pull t3code` would. When t3code mounts
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
  pull --remote t3code` and `newgit pull t3code`, one commit here. The
  cold-start measurement doubles as the profile cost check.

---

## Build order

Status as of 2026-11-05. Each step lands alone and keeps the suite green.

**Before roots (done, 2026-10):** tracker repos with real history (`main`,
`instances/<instance>`) in the shared store; the `root` tracker, ownership by
specificity, and the `git` flag; the projection (compose/split engine,
trackers in Git, wiring into spawn, push, and checkpoint sync, the remote
with one Artifacts repo per tracker and the Git endpoint); roles, priority,
`requires`, resources, `use`/`drop`; `newgit merge [instance] [--into
<branch>]` moving every tracker together; the GitHub export
(`.newgit/exports/<name>.toml`, `branches = true`). The engine of that
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
6. **Later:** spawning at a tracker (`spawn --root`), `.gitignore` as
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
  trackers, so a public tracker (t3code) could not be left out.
- **A root commit brought in from a mirror names the mirror's commit,** not
  the view commit rebuilt from it, so its id survives the round trip.
- **The project root learns what `git pull` brought.** The view commit of
  such a root commit is not what its trailers name, so the first sync after a
  pull composes the remote's `main` here, which notes it. A workspace that
  `git pull`s `origin` instead of `newgit pull` does not, and its split makes
  a commit of its own, which merges.
- **A mirror's event lands as a merge by whoever pushed it:** they must be a
  member and maintain what it changes (*The GitHub webhook*).
- **A mirror's commit that may hold withheld work is refused.** A pull
  request's branch leaves out what the target withholds, so landing it would
  lose an instance's edit there. The merge goes through `newgit merge
  <instance>` instead, and a mirror's merge then takes each withheld tracker's
  files from whichever parent is ahead, not its first
  ([remote.md](remote.md), *The GitHub webhook*).
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
  `newgit pull <tracker>`. For a target without `root`, the files of each
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

## Open questions and known gaps

Unbuilt parts of the design above are marked *planned* where they appear:
the `merge` and `production` keys, spawning an instance on another with
`requires` checked between them, spawning at a tracker, the `newgit branch`
CLI, the narrow-audience warning, `.gitignore` as definition, and
`depends_on` naming a role. Open issues on Spheroman/newgit:

- **Imported histories.** Import (#140), export as a root (#141), and pull
  back (#142) are built; bringing the real t3code in, and measuring its
  cold start, is #144.
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
- **The model itself.** The rest of #201: every tracker the same kind of
  thing, rooted at the user. The engine is built; what remains is step 6
  above, and the per-remote redesign under *Users, remotes, and trust*.
