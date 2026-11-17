- `newgit cleanup` collects what eviction leaves behind, only what is
  proven to be nobody's only copy: an `evict` checkpoint a restore brought
  back, once a later eviction supersedes it; the checkpoints a restore that
  succeeded took on its way; a `remove` checkpoint a retried removal
  superseded; and checkpoint refs no record names
  ([#446](https://github.com/Spheroman/newgit/issues/446)).

  Each `stop --evict` / `start` cycle used to add an evict checkpoint and
  one or two before-undo checkpoints that nothing ever pruned, and a large
  untracked file stayed in the store for as long as any of them reached
  it, so a machine where T3 Code evicts on every settle grew without
  bound. `cleanup` now names each one it collects, deletes its record and
  store ref, prunes the tracker revisions only they held, and says how
  many bytes of store objects became unreachable. An `evict` checkpoint
  nothing restored is never collected (an instance removed while evicted
  keeps its work only there, and a later instance with the same name
  shares its history), and neither is any checkpoint a live or archived
  record names, an `explicit` or `push` checkpoint, the redo point of an
  `undo` you ran, or what a failed restore took. A collected checkpoint is
  gone for `undo --to` too: keep a state with `newgit checkpoint`. A
  collected id is never reused.

  **`cleanup` does not prune or `git gc` the store.** The publish outbox
  (`.newgit/local/publish/outbox.git`) borrows the store's objects through
  alternates, so pruning the store can corrupt it. The store's own `git
  gc` frees the unreachable objects on its schedule, under your
  `gc.pruneExpire`, `gc.auto`, and `git maintenance` (two weeks by
  default, which is also how long `git fsck` can recover a dropped stash).
  `cleanup` holds each instance's lock only while it reads or deletes that
  instance's records.
