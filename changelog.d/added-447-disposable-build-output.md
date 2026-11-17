- A resource can declare ignored build output `disposable`
  (`disposable = ["target", ".next/cache"]`), and a plain `newgit stop
  --evict` then drops it with the workspace instead of refusing
  ([#447](https://github.com/Spheroman/newgit/issues/447)).

  Every Rust workspace held a `target/` no tracker owns and nothing
  rebuilds, so eviction refused there, and `--discard-ignored` was the
  only way through: it discarded every undeclared ignored file, a stray
  `.env.local` included. The declaration is a resource with no actions,
  identity, or hooks, so no `undo`, `tracker use`/`drop`, or `spawn`
  touches the tree, and one a tracker brings stops counting in an
  instance that drops the tracker. The eviction names what it dropped
  (`disposed` in `--json`, and `status` while evicted), and the refusal
  now names the declaration. `newgit remove` lists what it drops the same
  way.
