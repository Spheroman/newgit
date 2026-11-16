- `newgit remove` checkpoints the workspace before it deletes it, so it
  never deletes work no checkpoint holds
  ([#448](https://github.com/Spheroman/newgit/issues/448)).

  It used to delete a dirty workspace whatever was in it and report only
  `kept: N checkpoints`, which read as though the work were kept. Now
  uncommitted, untracked, and staged work is in a checkpoint (reason
  `remove`) that the archived record names as `removed_to`, and `newgit
  spawn <name> --from-checkpoint <name>/<id>` brings it back. What no
  checkpoint holds (ignored files nothing rebuilds, the stash, branches
  only the workspace has) is named as `lost:`. A checkpoint that fails
  refuses the removal, deleting nothing, unless `--force`; `--purge` takes
  no checkpoint and names the uncommitted work it deletes. `remove --json`
  is new, and reports `checkpoint` and `lost`. Callers such as T3 Code's
  thread deletion need no change: a dirty workspace no longer fails or
  prompts, it is checkpointed, and the checkpoint never waits on the
  remote.
