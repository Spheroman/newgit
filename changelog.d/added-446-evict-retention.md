- `newgit cleanup` collects what eviction leaves behind: every `evict`
  checkpoint but each instance's latest, and the checkpoints a restore
  that succeeded took on its way, with the store objects only they reach
  ([#446](https://github.com/Spheroman/newgit/issues/446)).

  Each `stop --evict` / `start` cycle used to add an evict checkpoint and
  one or two before-undo checkpoints that nothing ever pruned, and a large
  untracked file stayed in the store for as long as any of them reached
  it, so a machine where T3 Code evicts on every settle grew without
  bound. `cleanup` now names each one it collects and the bytes of store
  objects only they reached, prunes the tracker revisions only they held,
  and runs `git gc --prune=1.hour.ago` on the store. `explicit`, `push`,
  and `remove` checkpoints, the checkpoint an instance is evicted to, the
  redo point of an `undo` you ran, and what a failed restore took are
  never collected. A collected checkpoint is gone for `undo --to` too:
  keep a state with `newgit checkpoint`.
