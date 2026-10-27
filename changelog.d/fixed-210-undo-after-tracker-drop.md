- `undo` to a checkpoint taken by `tracker drop --discard` no longer brings
  back a file that was deleted when the checkpoint was taken
  ([#210](https://github.com/Spheroman/newgit/issues/210)).

  Undo restored the source while the tracker was still dropped, when its
  paths were skip-worktree entries the restore leaves alone, so a deleted
  file stayed in the index and came back from HEAD when the tracker was
  bound again afterwards. The binding is now restored before the source,
  so the restore sees the tracker's paths as ordinary tracked files and the
  deletion holds, as it already did for paths outside a dropped tracker.
