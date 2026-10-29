- `tracker drop`, and `tracker use` on a role alternative, work for a
  tracker with no `paths`, and `status` lists only the trackers an instance
  binds ([#246](https://github.com/Spheroman/newgit/issues/246)).

  Dropping a secret tracker captured it from the workspace first, and a
  tracker with no `paths` (deposit-only, or one that only brings resources)
  has nothing there, so the capture refused and the tracker could be bound
  but never unbound — the branch database you switch on for a migration and
  off afterwards needed a `remove` and respawn. There is now no capture step
  for such a tracker: its deposits are already in its lane, and a checkpoint
  covers them. `drop` stops what it brings, and `--stash`/`--discard` have
  nothing to act on. An explicit `tracker capture` still says there is
  nothing to capture. `status` showed every tracker in a role, including the
  dropped alternatives; it now shows what the binding record binds.
