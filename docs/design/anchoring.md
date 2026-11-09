# Anchoring

**Status: draft, for review.** Nothing here is built. It implements the
anchoring paragraph of #201's vision, and replaces "the root is the spine"
(`trackers.md`). There are no projects in this design, only a tree of trackers.
Where today's code has them (`ProjectDO`, `<owner>.<project>.t.<tracker>`), that
is storage to replace, called out under *Build order*.

## The problem

Everything lands through root. A merge has to move `root` (the remote refuses
one that doesn't), it compares the branch's whole tracker list, and it holds one
lock on the branch across its pushes, for up to ten minutes. So two changes to
unrelated trackers still queue, and any change anywhere sends every other merge
back to start again.

## The model

**The log is the truth, and commits are checkpoints of it.**

A commit lands on the lowest tracker covering everything it touches, its
*anchor*. Landing moves that tracker's head and appends to a log for its parent.
That is the moment the change is true.

A parent's state is always defined: its last commit plus what is queued for it.
It just may not be written down yet. Rolling up writes it down, and it does that
when something needs it. A landed change is safe at its anchor however long that
takes.

Creating commits needs no coordination, because they are content-addressed. Only
a head moving does, and that is one compare-and-swap.

## Decisions

Agreed with Jack, 2026-11-06:

1. **A parent never follows its children live.** Every commit stays a lockfile.
2. **Auto-merge unless there is a real conflict.** A commit based on an older pin
   merges with the tracker's head when no file overlaps. No rebasing.
3. **A parent rolls up when something needs it,** not on a schedule and not at a
   boundary. See *When a parent catches up*.
4. **History is a choice each parent makes about each mount:** `commits` or
   `pins`. See *How much history rolls up*.
5. **A failed roll-up serves the last commit, and says it is behind.** A read
   never fails because a parent has not caught up.

## Landing

1. **Find the anchor.** The lowest common ancestor, in the tracker tree, of every
   tracker the change touches. `split` already knows which those are. A change
   touching one tracker anchors there. One spanning `app` and `lib` anchors at
   their common ancestor, however high that is.
2. **Send the objects,** with no lock. Commits nobody references yet can't
   conflict with anything.
3. **Compare-and-swap the anchor's head** from what the work is based on to the
   new commit. If the head moved, merge and try again: Git's merge when files
   don't overlap, and a refusal naming the files when they do.
4. **Append to the parent's log,** and answer. The change has landed, and a busy
   parent delays nothing.

**Who approves.** The maintainers of the trackers a change touches. The anchor
only says where the commit sits: landing across `app` and `lib` asks both, not
whoever maintains the directory above them. Rolling up asks nobody, because the
change was already approved where it landed. So a tracker's maintainers need no
rights on anything above it.

**Atomicity.** A commit spanning trackers moves their pins in one commit at the
anchor, so no read can see half of it. Trees whose heads are stored apart can't
share one compare-and-swap, so a spanning commit takes a short reservation on
each head, in sorted order, then commits. That is the one slow path, and only
spanning commits take it. Where heads are stored is a storage detail, and nothing
above that layer should depend on it.

## When a parent catches up

Nothing decides eager or deferred. It follows from use:

- **A read finishes what is pending** before it answers, so landed work is always
  visible to the next `git fetch`, and the lag never reaches Git.
- **What follows a path drives it:** a mirror, or a machine on `/env/prod`. Those
  are subscriptions, and they roll up as soon as something lands.
- **A background sweep keeps queues short,** purely to bound the cost for the
  first reader of a tree nobody has read in a month.

A tree nobody reads and nobody follows stays unwritten. Every pin it has is still
correct, so "root is up to date" stops being true by default, and becomes true
when someone looks.

Rolling up works deepest first. If two readers race, they do the same work and
one wins the swap; the other re-reads the same result. Once written, a commit is
permanent.

If a roll-up fails, the read serves the last written commit and says it is
behind. It is retried by the sweep.

`newgit status` names what is pending, such as `root: 3 changes pending`, so "not
written yet" never reads as "out of date".

## How much history rolls up

A mount is the parent's to define, so this is the parent's choice:

| `history` | The parent records | Fits |
|---|---|---|
| `commits` (default) | One commit per change, so `git log` is complete | Directories that change together: `t3code` in `newgit` |
| `pins` | A snapshot of pin moves, with no child's messages | Things that merely sit together: `/remote/users` |

**`commits` merges the root commits `split` already made.** A change arrives with
a root commit of its own, based on whatever it started from. Rolling up
fast-forwards to it, or merges it into the parent's head when that moved, which
is what parallel instances produce today. A view is built per root commit, so
this keeps each change as its own entry in `git log`, with its id and signature.
Each commit is built from what was recorded at landing, so reading it later gives
the same commit.

**Batching is at the head, not in the history.** A roll-up makes one commit per
change and moves the head once: a thousand changes are a thousand commits and one
compare-and-swap.

**`pins` skips what nobody saw.** A roll-up writes the latest heads as one
snapshot, so its cost doesn't grow with what is queued. Its history depends on
when it was read: a week unread is one snapshot, so that parent can't say what it
pinned on Tuesday. Each child's own history still can.

A roll-up cannot conflict: below the anchor it moves only pins, and a pin's new
value is that tracker's head.

## Follow and pinned

Whether a mount rolls up at all is what `follow` already says:

- **Following a ref** (`t3code` on `main`): rolls up.
- **Pinned** (a private dependency, `/env/prod`): never moves by itself. Promotion
  is a commit moving the pin, and rollback is a revert.

`follow` names a ref to pull, so "pinned" is a key of its own: `pinned = true`
in the child's definition, which is the parent's choice, as the mount is.
On a branch a pinned child's head is the pin, since nothing moves one
without its parent. Its own development goes on its `main`. A merge that
moves the pin counts as touching the parent, so it lands there and the
parent's maintainers approve the promotion. A roll-up never moves the pin.
`newgit status` says when a pinned child's own `main` is past its pin.

A state holding two independent changes may never have run in CI. Anything that
needs a tested state should be pinned.

## Git

Git never sees a partial advance. It sees a view: a complete tree rooted at one
tracker. A ref moves when that tracker's head does, so every commit Git sees is
whole, and `bisect`, `blame`, and `log` work as they do today. Nothing here
touches Git's object model.

Each root is its own repository to Git, as today. A commit spanning `app` and
`lib` appears as one commit with `app`'s half in a view rooted at `app`, another
with `lib`'s half in one rooted at `lib`, and one whole commit in a view rooted at
the anchor.

**GitHub.** An export is a view pushed to a mirror, so a mirror is a subscriber.
GitHub has no cross-repository atomicity, so a spanning commit is two pushes and
one can fail. It has landed regardless, so the failed export is retried: newgit is
the source of truth, and a mirror may trail it.

## Build order

1. Fuzz test compose and split round trips. Everything here leans on them.
   **Done** (`crates/newgit-core/tests/compose_fuzz.rs`).
2. Per-tracker heads and a log, behind today's API, keyed by tracker path rather
   than by project. **Heads and the log done** (`ProjectDO`'s `heads` and
   `head_log`); keying them by path waits for step 8.
3. Landing without the lock: objects first, then one compare-and-swap, still
   always at root. This alone removes the queue. **Done**: commits wait at
   `refs/newgit/landing/<commit>`, `ProjectDO.land` is the compare-and-swap,
   and the Worker moves the repos' branches after it (`remote/src/follow.ts`).
   Merges and webhook pulls no longer lock.
4. Anchors, roll-up on read, and subscriptions. `merge` stops having to move root.
   **Roll-up done** (`remote/src/rollup.ts`). The Worker writes a parent's
   next commits itself, with no container. They are its tree with pins under
   `.newgit/pins/` replaced, one commit per change, deepest first. A change's
   own commit in the parent is kept: fast-forwarded to when it sits on the
   head, merged when the head moved on. The objects are encoded as Git
   encodes them (`remote/src/objects.ts`, checked against hashes Git made),
   pushed as a pack, and land by one compare-and-swap of the parent's head
   (`ProjectDO.rolledUp`). `ProjectDO` logs which commit a change made in its
   parent (`head_log.via`) and how far each parent has rolled up
   (`rollups`). **Landing at the anchor done.** The remote works the anchor
   out itself, at the finish (the client's commits arrive after `begin`). A tracker is touched when its commit changed
   anything besides `.newgit/pins/`, which three tree reads tell. Adding or
   dropping a tracker, or anything it cannot read, lands at root.
   - The base is checked, and the heads compare-and-swapped, only inside the
     anchor. The anchor's move is logged for its parent with the change's
     commit there (`via`).
   - Approval comes from the touched trackers' maintainers; above the anchor
     nobody is asked. The client sends its commits before `begin`
     (`POST /objects/<tracker>/<commit>`, a thin pack the Worker pushes to
     the landing ref itself), so `begin` reads them, works out the anchor,
     and asks the right maintainers. It hands out no write password then:
     the Worker moves every ref. A pack too large for the Worker (90 MiB),
     or a remote from before uploads, takes the old way, and `begin` asks as
     for root.
   - Reads roll up first: the project, `GET /branches`, a branch, the Git
     endpoint, and a webhook pull. One that cannot serves root's last commit,
     `behind`. Each landing also rolls root up after its answer, as the
     sweep's first form.
   - Subscriptions wait for exports to live on the remote: today the client
     exports.
5. `history`, and the sweep. **Done.**
   - A definition's `history` (`commits` by default, or `pins`) is how its
     parent records it. A `pins` child rolls up as one snapshot of where it
     is now, with none of its messages; `commits` children still get one
     commit per change (`remote/src/rollup.ts`).
   - The client merges tracker by tracker when the instance changed nothing
     of root's own but pins (`merge_by_pins`). Under `pins`, root's history
     never descends from the merger's, so a merge in the composed view met
     an old base and conflicted on files only the instance had changed.
   - The sweep (`remote/src/sweep.ts`) runs from `ProjectDO`'s alarm within
     the hour of a landing or an upload, and again while anything is left.
     It rolls up every pending branch. It deletes landing refs whose commit
     is in history at once, and others on their third sighting (two hours
     on, past any merge still pending).
6. `status`, exports, and the webhook follow the log. **Done.**
   - `GET /v1/projects/<p>/state` says what is not written down yet, without
     rolling it up: per branch, how many moves each parent has pending, and
     why its last roll-up failed. It also lists the webhook pulls waiting
     (with their last error) and how the last ones ended. `newgit status`
     says it when there is anything; `newgit remote show` always does.
   - A webhook pull lands at its anchor, as a merge does. A refused or
     given-up pull is said there, not only in the logs.
   - An export rooted at a tracker already follows the remote: it fetches
     the tracker's `main`, which the remote moves after each landing.
     `export_rooted_follows_remote.rs` holds it to that.
7. `follow` says pinned. **Done**, as `pinned = true` (see *Follow and pinned*).
8. Replace `ProjectDO` and project-shaped repo names, as #201's remote redesign.
   Designed in *Step 8: the remote by path*. **Items 1–3 done:**
   - `TrackerDO` per tracker path (`remote/src/tracker-do.ts`), with
     two-phase landing across several trackers.
   - Heads handed over lazily from `ProjectDO`.
   - `/<owner>/<name>/<path>.git` serves a view rooted at that tracker.

   One tracker in two roots comes next.

## Step 8: the remote by path

Agreed with Jack, 2026-11-08: members stay on the root, and one tracker
in two roots comes after step 8. Build order items 1–3 are built.

Today the remote is project-shaped. `ProjectDO`, one Durable Object per
`<owner>/<project>`, holds every tracker's heads, its log, and roll-up
bookkeeping, alongside members, instances, forks, checkpoints, and webhook
pulls. Repos are named `<owner>.<project>.t.<tracker id>`. Git serves only
views rooted at root (`/<owner>/<project>.git`). #201 asks for three things:
atomic merges without a project object, naming by path, and every tracker
with a `.git` endpoint of its own.

### What a project becomes

There are no projects. A *root* is a tracker with no parent, at a top-level
path (`spheroman/newgit`). Everything a project held either belongs to a
tracker or to a root:

| Today on `ProjectDO` | Becomes |
|---|---|
| heads, log, roll-ups | each tracker's own (below) |
| members | the root's readers and maintainers: `project-devs` means them, as it does today |
| instances, forks, checkpoints | the root's: an instance is a working copy of a root |
| webhook pulls | the tracker an export target roots at |
| pending merges | the anchor's |

### One Durable Object per tracker path

`TrackerDO`, named by the tracker's path (`spheroman/newgit/t3code/repos`),
holds the tracker's heads (one per branch), its log, an *inbox* of its
children's moves (what it rolls up), and an *outbox* of its own moves (to its
parent's inbox). A root's `TrackerDO` also holds what the root owns (the
table above).

Every tracker's heads live in its `TrackerDO`, root's included.
`ProjectDO`, named `<owner>/<project>` like the root's path, keeps only what
a root owns (the table above), plus the list of which trackers each branch
has, so it knows which objects to ask. It coordinates a landing without
holding anything: correctness comes from each tracker's compare-and-swap
and reservations. Heads an older remote kept in `ProjectDO` move out
lazily. The first time a tracker's object is asked for a head it has none
of, it adopts the one `ProjectDO` kept (`handOff`). Renaming the class to
say what it is now is cosmetic, and left out.

### Landing across Durable Objects

- **One tracker** (the common case: the anchor moves, nothing inside it
  does): one compare-and-swap in the anchor's object, then an outbox entry
  for its parent. Nothing else is touched.
- **Several** (the anchor and trackers inside it): two-phase.
  1. *Prepare*, in sorted path order: each tracker checks its head is the
     base and holds a reservation (new value, the anchor's path, a token,
     and a time limit).
  2. The decision goes to the anchor: *committed*.
  3. *Commit* each reservation.

  A read that meets a prepared reservation asks the anchor what it decided,
  so no read sees half a landing. A reservation whose time runs out without
  a decision is aborted. This is the one slow path; only spanning landings
  take it.
- **The outbox** is delivered to the parent's inbox at least once, and the
  parent dedupes by `(child, seq)`. A tracker's first head (from nothing) is
  not sent: a landing at the parent or above set it, so it is pinned already. The landing has happened once the
  anchor's head moved. A delivery that fails is retried by the anchor's
  alarm, so a parent never misses a move.

Roll-up and the sweep work as they do now, per object. A parent rolls up
from its inbox, and moving its head puts an entry in its own outbox.

### Naming and Git

- **Paths everywhere a person sees one:** `git clone
  https://newgit.dev/spheroman/newgit/t3code.git` serves a view rooted at
  `t3code`, composed from its head (#201: "every tracker gets a `.git`
  endpoint"). `/spheroman/newgit.git` stays the root's view.
- **Repo names stay** (`spheroman.newgit.t.t3code__repos`): a storage
  detail, already a function of the path. Renaming them would copy every
  history (t3code's is 365 MiB) for nothing a person sees.

### Build order within step 8

1. `TrackerDO` with heads, log, inbox, and outbox; `ProjectDO` renamed to
   it; children adopted lazily. One-tracker landings and roll-ups through it.
   Tested on the dev remote with a copy of the live project's shape.
2. Two-phase landing for spanning merges.
3. Path-addressed Git endpoint (`/<root>/<path>.git`).
4. A tracker bound in two roots (#201's suggested experiment): a second
   mount is a symlink to the same `TrackerDO`, and its parents both get its
   outbox entries.

### Questions for Jack

- **Members:** keep a root's member list as its readers and maintainers
  (`project-devs`), or should membership itself become per tracker now?
  Recommended: keep it on the root for step 8, and make it per tracker when
  #201's audience item ("readers or maintainers of tracker X") is built.
- **Build order item 4** (one tracker in two roots): in step 8, or after?
  Recommended: after, since it needs nothing above it to change.

## Open questions

- **Reservations across trees.** How long may one be held, and what breaks one a
  dead client left behind?
- **Pins and privacy.** A `pins` parent carries no messages, but a pin still says
  a tree exists (#201, "the registry leaks shape").
- **The webhook.** Resolved: a pull lands at its anchor, as a merge does.
- **Permission to anchor.** Who may land at a tracker when the change was reviewed
  on a mirror that sees only one view?
- **Instances behind.** `status` counts commits behind the store's own
  branches. It says the remote's pending roll-ups beside them now, but an
  instance's own count still knows nothing of a tracker that moved on the
  remote and not here.
- **Where a merge runs.** Git's merge needs the container, the slow part. Roll-ups
  move only pins, and may not need one.
