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

`follow` today names a ref to pull, so saying "pinned" needs a value of its own.

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
   than by project.
3. Landing without the lock: objects first, then one compare-and-swap, still
   always at root. This alone removes the queue.
4. Anchors, roll-up on read, and subscriptions. `merge` stops having to move root.
5. `history`, and the sweep.
6. `status`, exports, and the webhook follow the log.
7. `follow` says pinned.
8. Replace `ProjectDO` and project-shaped repo names, as #201's remote redesign.

## Open questions

- **Reservations across trees.** How long may one be held, and what breaks one a
  dead client left behind?
- **Pins and privacy.** A `pins` parent carries no messages, but a pin still says
  a tree exists (#201, "the registry leaks shape").
- **The webhook.** It lands on root today. A merge on a mirror touching one tracker
  should anchor like any other.
- **Permission to anchor.** Who may land at a tracker when the change was reviewed
  on a mirror that sees only one view?
- **Instances behind.** `status` counts commits behind root, which now means
  something narrower.
- **Where a merge runs.** Git's merge needs the container, the slow part. Roll-ups
  move only pins, and may not need one.
