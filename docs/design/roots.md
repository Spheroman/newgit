# Roots and mounts

*Design for #201: no projects. There is one kind of tracker; an instance is
rooted at whichever tracker you spawn at; one tracker sits inside another by
a mount, and the parent records which of the child's commits it holds. This
document settles what compose, split, and merge need and takes a position on
the rest of #201. Decided with Jack on 2026-11-04 (see the end). Once built,
it is folded into [trackers.md](trackers.md), which it partly supersedes.*

*Terms: a tracker **in Git** (`git = true`) is committed with Git and split
into its history; one **outside Git** is captured by newgit and never
committed. That flag replaces `secret` and lands first, on its own; see
Decided, item 2.*

---

## Why compose changes

Compose today interleaves every tracker in Git by committer date, using
`Newgit-Split-Parent` trailers to recover the order in which commits split
from one view were made. That works for trackers carved out of one project,
because every commit came from that project's view and knows where it sat.
It fails as soon as a history comes from anywhere else:

- An outside repository's commits (t3code, 2026-02 onward) have no position
  in newgit's view. Placed by date, they land under newgit's own commits and
  every id after them changes.
- #151 placed them with *join markers*: commits on t3code's own history
  naming the newgit view commit to join after. Three review rounds later the
  rule for which branches a marker joins was still wrong: a pre-import branch
  merged into `main` un-joined it, `tracker repair` broke markers, and the
  order of the import's own writes mattered. Each of those bugs came from
  working out from the child's history something only the parent knows.
- Split follows the first parent, so a merge inside the work being split is
  flattened, and an instance commit touching two trackers comes apart when
  merged into a `main` that moved.

#201 names the fix in passing: **the parent decides the mount.** This design
takes it literally. The parent's history records which commit of the child
each parent commit holds. Compose reads the parent's history and expands it.
Nothing is inferred.

## The model in one page

- **A tracker** is a history of files stored relative to its own root, and a
  definition: where it is mounted, what it claims, who may read and maintain
  it, the resources it brings, and whether it is in Git.
- **A mount** places a tracker at a path inside another tracker.
- **A pin** is the commit of a mounted tracker in Git that a parent commit
  holds, stored in the parent's tree as a Git *gitlink* (the object Git uses
  for a submodule's commit) at `.newgit/pins/<name>`.
- **A root** is the tracker an instance is spawned at. The instance's `.git`
  is the *view*: the root's history with every pin expanded into files under
  its mount, recursively. Rooted at `newgit` you get newgit with
  `apps/t3code` inside it; rooted at `t3code` you get t3code alone.
- **A view commit is one root commit.** Each commit of the root's history
  composes to exactly one view commit, and splitting a view commit makes
  exactly one root commit. The root's history is the view's shape.

```
   newgit (root)                              t3code
   ─────────────                              ──────
   R3  split from V3, pins t3code at C2  ◄──  C2   (V3's t3code half)
   R2  newgit pull t3code: pins X1       ◄──  X1   (merged on Spheroman/t3code)
   R1  adds the mount, pins C0           ◄──  C0 ◄── … 7,800 commits from outside

   view rooted at newgit:
     V3 ── V2 ──────── V1 ──────── newgit's history …
           └ X1'       └ t3code's history, replayed under apps/t3code
```

A submodule in storage, a monorepo in every view. Agents never see a
gitlink, `.gitmodules`, or a nested `.git`.

---

## Storage

**Every tracker stores files relative to its own root.** `apps/t3code/x.ts`
in the view is `x.ts` in t3code's repository. That is what lets one tracker
be mounted by two parents, or be a root itself, and it is the shape an
imported repository already has, so importing rewrites nothing.

**The definition** stays one file per tracker, `.newgit/trackers/<name>.toml`,
owned by `access`. It gains two keys:

```toml
# .newgit/trackers/t3code.toml
audience = "public"
storage  = "remote"
git      = true
mount    = "apps/t3code"     # where its root sits; default "."
follow   = "main"            # the ref `newgit pull t3code` brings in; default "main"
paths    = ["."]             # claims, relative to the mount
```

`role`, `priority`, `requires`, and `resources` stay where they are. Moving
them to the mounting parent (#201 item 4) is a later change; compose doesn't
need it.

**Pins.** Each commit of a parent holds a gitlink at `.newgit/pins/<name>`
for every tracker in Git that it mounts. Pins are left out of every view, so
no workspace ever holds one. They sit under `.newgit/pins/` rather than at
the mount because a tracker can be mounted at `.` (an overlay beside its
parent's files), and a gitlink cannot be the root of a tree. A gitlink
rather than a text file because it is Git's own object for "this tree holds
that commit". newgit resolves pins itself in every merge, so it never relies
on Git's handling of gitlinks. Gitlinks don't make Git keep the commit; the
tracker store's refs do, as for every tracker today.

Trackers outside Git are mounted (their definition has a `mount`) but never
pinned. Their state stays in the instance's binding record and in
checkpoints, as today.

**`project` folds into the root tracker.** `.newgit/` is ordinary content of
the tracker that holds it. `access` stays a separate tracker, mounted at
`.newgit/trackers`, maintained by the owner.

## Shape and policy

Compose needs the **shape** of each commit: which trackers it pins, at which
commits, mounted where. Everything that decides who may read or change
something is **policy**. They come from different places, on purpose:

- **Shape comes from the commit itself.** Its pins, and each pinned tracker's
  `mount` as that commit's own tree states it (in `access`'s content at that
  commit). A commit that moves a mount composes by the new one.
- **Policy comes from `access`'s own `main`,** never through a pin: who may
  read and maintain each tracker. The Worker already reads definitions this
  way (`loadDefinitions`). So `access`'s pin in the root exists only so that
  views reproduce. No one can roll policy back by moving a pin, or by editing
  an old commit.
- **Claims are read only by split.** Compose never reads them: each tracker's
  stored tree contains exactly what it owned when the commit was made, so
  overlaying the stored trees is enough, as it is today. Changing a claim
  (adding `!dist/`) changes where new work goes, never what old commits look
  like.

---

## Ownership

Every path in a view has at most one owner. Ownership comes from claims,
never from which tracker currently has the file, and it is the same
whichever tracker you root at. Split assigns paths by this rule:

1. **The deepest mount wins.** A path under `apps/t3code/` is decided among
   the trackers mounted at `apps/t3code` or deeper. A parent's claim of `.`
   does not reach into a mount.
2. **Among trackers mounted at the same path, the longest claim wins.**
   `local-db` mounted at `.` claiming `.env.db` beats newgit's own `.`.
3. **Identical claims at the same mount are an error, except within a role.**
4. **Claims can exclude**, `.gitignore`-style: `paths = [".", "!.env",
   "!node_modules/"]`. An excluded path is owned only by a tracker that
   explicitly claims it at that mount, or filled by a resource. Otherwise it
   is unowned, which is all "ignored" ever meant.

Rooted at `t3code` there is no parent, so the paths `t3code` excludes are
simply absent.

**The definition is the `.gitignore`.** Generating the view's `.gitignore`
from claims, and turning an agent's edit of it into a definition change, is
part of #201, but compose doesn't depend on it. It lands later.

---

## Compose

Compose is defined over views, recursively: a child's view (rooted at the
child) is composed first, and what a parent mounts is that view. A
grandchild's history therefore reaches the root exactly as it reaches the
child.

For a view rooted at tracker `T`, for a reader who may read some set of
trackers:

1. **Walk `T`'s history** in `git rev-list --topo-order --reverse --parents`
   order, every parent of every commit (today's order, now part of the
   spec, since byte-identical output depends on it).
2. **For each commit `R`, the view commit `V(R)` has:**
   - **parents:** `V(p)` for each parent `p` of `R`, in order, plus one more
     for each child that `R` brings history in from (step 3);
   - **tree:** `R`'s tree without `.newgit/pins/`, with each readable pinned
     child's view tree at its pin set under its mount (fast-import's
     `M 040000 <tree> <path>`, with no diffing);
   - **metadata:** `R`'s author, committer, dates, and message, newgit's
     trailers stripped and the recorded headers put back. When `R` holds
     none of `T`'s own changes, and a child commit holds the work (see
     *Split*), the metadata comes from that child commit.
3. **Inline, or brought in.** For each pinned child, the child view commits
   *new* at `R` are those reachable from the child's view at `R`'s pin and
   not from its view at any of `R`'s parents' pins.
   - **Inline** when `R` was split from a view commit (it carries
     `Newgit-Split`): its new child commits are the other halves of the same
     work. Also inline when `R` is a merge and the only new child commit is
     a merge whose parents are `R`'s parents' pins: the child half of the
     same merge. Their changes are part of `V(R)`, and nothing is added.
   - **Brought in** otherwise: a mount added, a `newgit pull`, an upstream
     sync, a pin moved by hand. `V(R)` is a merge whose extra parent is the
     new child commits, *replayed*. Each replayed commit is the child's view
     commit with its tree under the mount and, beside the mount, the rest of
     the tree as its first parent holds it (the first parent being the
     replay of the child commit before it, or the view commit that first
     took that child commit inline, in this view's walk order). History from
     before the mount existed has nothing beside the mount. This is
     `git subtree add` and `git subtree pull`'s shape: `git log -- apps/t3code`,
     `git blame`, and `git bisect` stay real through the merge, and because
     inline commits are represented by their own view commits, nothing
     appears twice.

   A replay is a pure function of the child's commits and the view commits
   it starts from, so two branches that bring in the same upstream push
   replay it identically, and merging them shows it once. It is cached in a
   note keyed by both. A mount removed and added again replays the child's
   whole history again, as `git subtree add` would.
4. **Readers.** A child the reader may not read is left out of the tree, and
   so is everything mounted inside it. A single-parent view commit whose only
   change was to such a child is dropped, and the commit after it moves onto
   its parent. A merge is never dropped. Someone who cannot read a tracker
   sees different ids from the first commit that touched it, as today.
5. **Originals are reused.** When `V(R)`'s parents, tree, and metadata come
   out as the view commit `R` was split from (its `Newgit-Split`), compose
   emits that object, signature and id intact. A composer without it
   rebuilds it and puts the recorded headers back (#291's mechanism,
   unchanged).

**What goes away:** interleaving by date, the group scheduler,
`Newgit-Split-Parent`, join markers and `Newgit-Import`, `Newgit-Follows`,
the "went on from" rule, and compose reading definitions at the tip. A
branch that never added the mount never has t3code, and merging `main`
brings the mount in through Git's own merge.

**Full replay vs. root as spine.** On 2026-10-04 we chose full replay over
"root as spine", for real `git log` and `git blame` everywhere and stable ids
across a split. This is a spine that keeps both: child changes made here are
inline in the view commit they were made in, and the one-to-one mapping from
root commits to view commits is what makes ids stable. The old spine
proposal kept children as merges; this one does so only for history that
really came from elsewhere.

## Split

Splitting view commit `V` (work leaving the machine: push, sync, merge),
rooted at `T`:

1. **Walk every parent,** not only the first. A parent already split is
   found through the `Newgit-Split` index (the root commit naming it), so a
   merge of an already-pushed `origin/main` maps to the root commit it was
   split into. The refusals (paths owned by trackers outside Git, unbound
   paths, mixed audiences) apply to every commit sent, side branches
   included, as the push gate already does.
2. **Assign each changed path** to its owner, by the claims as `V`'s own tree
   states them.
3. **For each child, deepest first:** if `V`'s subtree for it equals the
   child's view at one parent's pin, and that pin descends from every other
   parent's pin, reuse that pin and make no commit. (Merging `main` into an
   instance that never touched t3code makes nothing in t3code.) Otherwise
   commit the child's part on the child's instance branch, on top of the
   child's pins in `V`'s parents (a merge where they differ), at the child's
   own root.
4. **Commit `R` to `T`** with `T`'s own changes, every pin moved to what step
   3 decided, and parents mapped one-to-one from `V`'s parents. A merge in
   the view is a merge in the root, so merges inside the work are no longer
   flattened, and an instance commit touching two trackers stays one commit
   wherever it is merged.

**Which commit carries the message.** `V`'s author, dates, and message go on
the commit that holds the work:

- **`R` holds `T`'s own changes, or no child commit was made** (an empty
  commit, a merge where every child's pin was reused): `R` carries `V`'s
  metadata, with `Newgit-Split: <V>` and a `Newgit-Split-Header:` per
  recorded header. Any child commits carry the same author, dates, and
  message, without trailers.
- **`R` holds only child changes** (one child or several): every child commit
  carries `V`'s metadata without trailers, and `R` gets a fixed message
  (`Update t3code`), the child commits' dates, and the trailers. Compose
  takes `V`'s metadata from the child commits, which are identical. The
  fixed message never reaches a view.

This keeps every tracker's own history clean: rooted at a tracker that
mounts nothing in Git (t3code today), its commits are shown as themselves,
and Spheroman/t3code receives ordinary commits, not ones ending in
`Newgit-Split:` lines. It also lets a narrow child be edited from a wider
root: the message stays with the child.

**One commit, one audience.** When more than one commit carries `V`'s
message (the first case with child commits, or several children in the
second), all of their trackers must share one audience. Otherwise split
refuses, naming each tracker's audience, as today's `MixedAudience` does,
in either direction.

**Signatures.** Unchanged from trackers.md: the recorded headers restore the
original's signature wherever the rebuilt commit hashes to `V`. Child commits
are signed as their author when `commit.gpgSign` is set.

**Carving is a commit.** `newgit tracker create electron --git --path
apps/electron` makes one root commit that removes the files from the root's
tree and adds the definition, the mount, and a pin to the child's first
commit, which holds those files at its own root. History before the carve
stays in the root, where the view already shows it, so nothing is rewritten
and no id changes. The cost: rooted at `electron` alone, its history starts
at the carve. Carving with history can come later as an explicit rewrite if
a case needs it.

---

## Merges and pull

**A merge is a view commit, split like any other.** `newgit merge <instance>
[--into <branch>]` makes the merge commit in the merger's view (parents: the
branch's view tip and the instance's), with each child merged tracker by
tracker as today (`git merge-tree`; a conflict moves nothing), and splits it
the ordinary way. The root merge carries `Newgit-Split`, so a merger's signed
merge commit comes back as itself.

**The commit point is the root's ref.** A branch is a ref on the root
tracker's repository. Its view follows from that one commit, and on the
remote, ProjectDO's list for the branch becomes an index written after the
ref moves.

1. Before anything is pushed, the Worker is told every tracker whose pin
   moves to a commit not on that tracker's `main`, and refuses a merger who
   doesn't maintain it. This is the only place pin moves are checked, so it
   is enforced here and not in the client.
2. The root's ref moves by one compare-and-swap. If it loses the race,
   nothing has changed and the merge reruns on the new tip.
3. Each child's `main` then moves to include what the root now pins, also by
   compare-and-swap. If a child's `main` moved meanwhile (a merge on
   Spheroman/t3code), newgit merges the two and moves the child's `main` to
   the result. The root picks that merge up at its next `newgit pull`. The
   views are coherent throughout: the root pins exactly what was merged, and
   a view rooted at the child lags until its `main` catches up.

**`--only` is dropped.** With the root as spine, moving the root's `main` to
an instance's commits moves every child those commits touched. You can't
merge `supabase`'s half of a commit and leave `electron`'s, any more than in
a Git monorepo: a partial merge would be a cherry-pick, with new ids. Work
that should merge separately goes in separate instances, and `requires` is
checked between instances (`spawn --from <instance>`, planned).

**Pulling a child is explicit.** A child's `main` moves when an instance
rooted at it merges, when `newgit pull --remote` brings in a PR merged on
GitHub, or when an upstream is synced. The parent sees it when someone runs:

```sh
newgit pull t3code
```

That makes one view commit, `Pull t3code: 14 commits from main`, which moves
the pin and composes as a merge (*brought in*, above). It is authored by
whoever ran it, needs the root's maintainers like any change, and merges into
`main` like any instance commit. `follow` names the ref it pulls. Spawn never
writes a commit, but it says when a mounted tracker is behind its `follow`
ref, and `newgit merge` of an instance rooted at a child names the parents
that mount it.

Parallel instances merge the way a monorepo does: two instances that each
made a commit produce a merge commit in `main`'s view, even if they touched
different trackers. That is what Git would show.

---

## Rooting at any tracker

**Spawning at a tracker:** `newgit spawn --root t3code` composes the view
rooted there. Resources come from the trackers in that view. A tracker that
mounts nothing in Git composes to its own history unchanged. One that does
composes like any root (its pin-only commits rebuilt with their children's
metadata), so its view is no longer its raw history. While that is so, it
can't be exported as its own root (#141 refuses, naming the child).

**A root that `requires` its parent** (rooting at `electron`, which needs the
workspace `package.json`) is an error naming the parent to root at instead.
Rooting at the parent automatically would quietly give you a repository you
didn't ask for.

**The same tracker mounted twice in one view** (newgit mounts `ui-kit@v2`,
and `design-system` mounts `ui-kit@v1`) is allowed. Each parent pins its own
copy, so the two copies sit at their own commits, which is what #202 needs
for packages (one version per dependent, as `node_modules` nests). An edit
at either path splits into `ui-kit` on top of that copy's pin. A commit that
edits both copies is refused: `ui-kit is mounted twice; edit one`.

**Layering** (a lower tracker supplies defaults, the upper takes the writes)
is not built. Separate file names (`.env` and `.env.local`) cover every case
found so far, and layering needs whiteouts.

## Users, remotes, and trust

None of this is needed for the compose rewrite or for t3code. These are
positions, so the rewrite doesn't paint them into a corner:

- **A profile is a root that mounts your projects,** plus an *overlay*
  tracker, bound into instances, that only fills paths other trackers
  exclude. Keeping them as two trackers avoids the newgit ⊃ jack ⊃ newgit
  cycle without a special binding rule.
- **Cost of composing a profile:** each tracker's view is composed once and
  memoized, so a profile costs the sum of its projects once, plus the
  profile's own commits. Measured with t3code as part of #144.
- **Code enforces, trackers hold policy.** Reading policy from `access`'s
  `main` is the first instance. Per-remote trackers, home trackers for
  branch heads, and per-audience registries stay as the #201 comments
  propose. They are a remote redesign, later. The root's ref as the commit
  point already answers #201 item 3 for one root.
- **Remote naming** flattens to `<owner>/<tracker>` with that redesign. The
  compose rewrite keeps today's naming.
- **A fork keeps its upstream's audience as a ceiling** (#227), unchanged.

## #202

The four #202 decisions this design has to leave room for:

- **Mounts accept a ref:** pins hold exact commits and `follow` names the
  ref, so `ui-kit@v2` is a `follow` value. #202's "pin by forking" is no
  longer needed: every parent commit is already a lockfile.
- **Resources fill excluded paths:** ownership rule 4.
- **Content-keyed resource outputs:** a resource keyed by the Git tree id of
  the paths it reads is shared by any two commits with that content.
  Independent of compose.
- **Resources declare permissions:** independent of compose.

---

## Migration

Today's trackers in Git are `root`, `project`, and `access`; `site` and the
rest are outside Git. Since carving is a commit, no view history is
rewritten:

1. Fold `project` and `access` back into the root tracker. The root's
   history becomes the view's history, commit for commit, which is what the
   view already is, so every view id is unchanged by construction.
2. Carve `access` again with one commit at the tip of `main`: mount
   `.newgit/trackers`, `follow = "main"`.
3. **Gate:** recompose every branch and checkpoint in a fresh repository
   holding only tracker commits (the remote's situation), and require the
   same ids, signatures included, since most of `main` is signed by GitHub.
   A check run here would pass trivially, because local compose reuses
   originals.

This force-pushes every tracker repository on the remote, and rewrites each
branch's list as the new index. Jack is the only user, so no compatibility
path is kept. `tracker repair` and carving with history are deleted.

## The t3code milestone on this design

- **#139 (compose an imported tracker):** replaced by this design. #151 is
  closed unmerged; its tests carry over as acceptance cases (the join keeps
  ids, old branches don't get t3code until they merge `main`, upstream syncs
  go on top whatever their dates, log and blame through the join, non-UTF-8
  headers), plus the cases its last review found: a pre-import branch merged
  into `main`, and repair over a join.
- **#140 (import):** fetch the branch unchanged as the tracker's history,
  then one view commit adding the definition, the mount, and the pin (a
  *brought in* merge). #156's fetch and refusals carry over; its join wiring
  does not.
- **#141 (export a tracker as its own root):** push the view rooted at
  `t3code`. While t3code mounts nothing in Git, that is its own history, so
  the first export is a no-op fast-forward of Spheroman/t3code, ids and
  signatures intact. Refused while it mounts a tracker in Git.
- **#142 (pull from a target):** for Spheroman/t3code, fetch, merge into
  t3code's `main`, then `newgit pull t3code`. For Spheroman/newgit, its
  commits are view commits: ours are recognised by the canonical notes, and
  the rest are split as ordinary work.
- **#143 (`gh` per target):** unchanged.
- **#144 (bring t3code in):** Spheroman/newgit's export *includes* t3code
  (decided), so it stays a true mirror with the project's own ids, and
  `git pull` from GitHub in the project root keeps working. FORK.md:
  upstream comes in by merging pingdotgg/t3code into the fork's `newgit`
  branch, then `newgit pull --remote t3code` and `newgit pull t3code`. The
  cold-start measurement doubles as the profile cost check.

## Build order

1. **`secret` → `git`, alone,** before anything else, because it touches
   every definition and fixture (AGENTS.md: a shared rename goes first).
2. **Compose and split on pins,** alongside the current engine: the new
   `compose`, the new `split` and merge, #151's tests ported, and the
   migration's gate as the test everything else waits on.
3. **Migration,** run locally, then on newgit.dev.
4. **Delete the old engine:** date interleave, `Newgit-Split-Parent`,
   carving with history, `tracker repair`, workspace-path storage,
   `project`.
5. **The t3code milestone** on the new engine: #140, #141, #142, #143, #144.
6. **Later:** spawning at a tracker, `.gitignore` as definition, moving
   `role`/`priority`/`requires` to the parent, profiles, the per-remote
   redesign.

---

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
9. **The same tracker may be mounted twice;** a commit editing both copies is
   refused. Layering is not built. Rooting at a child that requires its
   parent is refused.
10. **Spheroman/newgit mirrors t3code too.**
