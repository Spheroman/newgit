- `newgit remove` checkpoints the workspace before it deletes it, so it
  never deletes work no checkpoint holds
  ([#448](https://github.com/Spheroman/newgit/issues/448)).

  It used to delete a dirty workspace whatever was in it and report only
  `kept: N checkpoints`, which read as though the work were kept. Now
  uncommitted, untracked, and staged work is in a checkpoint (reason
  `remove`) that the archived record names as `removed_to`, and `newgit
  spawn <name> --from-checkpoint <name>/<id>` brings it back. Removing an
  evicted instance takes no new checkpoint: it names the one the instance
  was evicted to, in `removed_to`, `checkpoint` in `--json`, and the
  `kept:` line. What no checkpoint holds (ignored files nothing rebuilds,
  the stash, branches only the workspace has, a Git repository nested in
  the workspace) is named as `lost:`, and `disposable` build output as
  `dropped:`. A resource capture that fails, such as a `pg_dump` against a
  database that is down, no longer refuses the removal, and its
  `[cleanup]` (a `dropdb`) no longer runs: the state is left in place,
  named as `left:` with the cleanup command, unless `--force`. `--purge`
  discards only the removed instance's checkpoints, never an earlier
  instance's of the same name, and archived records are never replaced
  by one archived in the same second. A checkpoint that fails, a nested repository
  with no commit, or a workspace whose Git cannot be read refuses the
  removal, deleting nothing, unless `--force`. `--purge` takes no
  checkpoint. Either way, the removal then names the uncommitted work it
  deletes and, per tracker outside Git, the paths that differ from its
  last capture. What it could not read is named as lost instead of
  refusing. `remove --json` is new, and reports `checkpoint`, `lost`, `left`, and
  `disposed`. Callers such as T3 Code's thread deletion need no change: a
  dirty workspace no longer fails or prompts, it is checkpointed, and the
  checkpoint never waits on the remote, not even to re-encrypt a tracker.
