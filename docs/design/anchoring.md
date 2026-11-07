# Anchoring

**Status: draft, for review.** Nothing here is built. It implements the
anchoring paragraph of #201's vision, and replaces "the root is the spine"
(`trackers.md`) for landing work, though not for reading it.

## The problem

Everything lands through root. A merge has to move `root` (the remote refuses
one that doesn't), it compares the branch's whole tracker list, and it holds
one lock on the branch across its pushes, for up to ten minutes. So two changes
to unrelated trackers still queue, and any change anywhere sends every other
merge back to start again.

## The idea

A commit lands on the lowest tracker covering everything it touches, and moves
only that tracker's head. Parents catch up afterwards, in a step that cannot
conflict. Creating commits needs no coordination, because they are
content-addressed. Only a head moving does, and that is one compare-and-swap.

## Decisions

Agreed with Jack, 2026-11-06:

1. **Eager roll-up.** A parent's pins move right after a change lands, not at
   the parent's next commit, and a parent's view does not follow its children
   live. Every parent commit stays a lockfile.
2. **Auto-merge unless there is a real conflict.** A commit based on an older
   pin merges with the tracker's head when no file overlaps. No rebasing.
3. **A parent may lag for a moment,** and `newgit status` says so: `t3code:
   rolling up`. Commands do not wait for it.

## Landing

1. **Find the anchor.** The lowest common ancestor, in the mount tree, of every
   tracker the change touches. `split` already knows which those are.
2. **Send the objects,** with no lock. They are content-addressed, so a push of
   commits nobody references yet can't conflict with anything.
3. **Compare-and-swap the anchor's head** from what the work is based on to the
   new commit. If the head moved, merge in the tracker and try again: Git's
   merge when files don't overlap, and a refusal naming the files when they do.
4. **Queue the roll-up,** and answer. The change has landed: a busy parent
   delays nothing.

A commit spanning trackers anchors at their common ancestor, which moves their
pins in one commit, so they land atomically. Every child moved there moves its
head by the same step.

Heads live in `ProjectDO`, one row per branch and tracker, so a compare-and-swap
is one row in a single-threaded object. The merge lock goes, and so does
comparing the whole list.

## Roll-up

`ProjectDO` keeps a queue per parent, and its alarm drains it, deepest first.

**Roll-up merges the root commits `split` already made.** A change arrives with
a root commit of its own, based on whatever `main` it started from. Roll-up
fast-forwards to it, or merges it into the parent's head when that moved, which
is what parallel instances produce today.

That matters for history. A view is built per root commit, so one parent commit
standing for several changes would collapse them into one entry in `git log`,
and the ids agents already hold would not come back out. Keeping the commits
keeps all of that, signatures included.

**Batching is at the head, not in the history.** A drain makes one commit per
change and moves the head once, so a thousand changes are a thousand commits and
one compare-and-swap.

A roll-up cannot conflict: below the anchor it moves only pins, and a pin's
new value is that tracker's head. Losing the race means reading the head and
going again. It needs no maintainer's permission, because the change was
approved where it landed, so t3code's maintainers can land t3code with no rights
on root.

## Follow and pinned

Whether a mount rolls up is what `follow` already says:

- **Following a ref** (`t3code` on `main`): rolls up.
- **Pinned** (a private dependency, `/env/prod`): never moves by itself.
  Promotion is a commit moving the pin, and rollback is a revert.

`follow` today names a ref to pull, so saying "pinned" needs a value of its own.

## Reading

Reading `main` serves root's head, so it lags by at most a drain. A tracker's
own head is always current: `git clone .../t3code` sees a change as soon as it
lands. Exports run after roll-up, from root's head.

## What stays

Compose and split, pins, and every id. Only when heads move changes. A project
with one tracker lands exactly as Git does.

## Build order

1. Fuzz test compose and split round trips. Merge is about to lean on them.
2. Per-tracker heads in `ProjectDO`, behind today's API.
3. Landing without the lock: objects first, then one compare-and-swap, still
   always at root. This alone removes the queue.
4. Anchors and the roll-up queue. `merge` stops having to move root.
5. `status`, exports, and the webhook follow roll-up.
6. `follow` says pinned.

## Open questions

- **A drain that fails.** A change has landed but not rolled up. Retrying is
  safe, since every step is idempotent, but how long before a person is told?
- **The webhook.** It lands on root today. A merge on a mirror touching one
  tracker should anchor like any other.
- **Permission to anchor.** Who may land at a tracker when the change is
  reviewed on a mirror that only sees root's view?
- **Instances behind.** A roll-up moves `main` under every instance. Nothing
  needs them to know, but `status` counts commits behind root, which now means
  something narrower.
- **Is step 3's merge in the container?** It needs Git, and the container is
  the slow part. Pin-only merges may not need one.
