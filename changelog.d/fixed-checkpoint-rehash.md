- `newgit checkpoint` no longer reads every tracked file to snapshot
  uncommitted work.

  The snapshot started from an index with no stat data, so `git add -A`
  hashed the whole tree. That was invisible at a few hundred files and took
  seconds once a project held thousands: on newgit with t3code mounted (29,164
  files), a checkpoint went from 2.3s to 0.45s and `undo` from 2.8s to 0.9s.
  It now starts from a copy of the real index, so only files that changed
  are hashed. What a snapshot contains is unchanged.
