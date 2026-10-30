- `spawn --from-checkpoint` now refuses `--use` and `--drop`, as it already
  refused `--from` and `--private`, rather than taking them and ignoring
  them.

  The instance binds what the checkpoint bound, because the undo that
  finishes the spawn restores the checkpoint's binding. So `--drop` left a
  tracker the checkpoint had bound in the working tree, with any
  uncommitted work the author had in it; `--use` left one it had not bound
  out; and a tracker name that does not exist, which a plain `spawn`
  refuses, was accepted. The spawn went ahead, `--json` too, with nothing
  in its output saying so. Either flag with `--from-checkpoint` is now a
  usage error, as the other two are: exit 2 before anything runs, a
  message on stderr naming both flags, and nothing on stdout. To bind
  otherwise, spawn without them, then `newgit tracker use` or `newgit
  tracker drop` in the new instance.
