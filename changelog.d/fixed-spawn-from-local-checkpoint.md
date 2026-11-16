- `newgit spawn --from-checkpoint` works with a checkpoint taken on this
  machine, with or without a remote.

  It used to go to the project's remote every time, so a project with no
  remote could not spawn from its own checkpoints. Now a checkpoint this
  machine has, even of an instance since removed, is used as it is, and
  only one it lacks is fetched from the remote. Text output says "taken on
  this machine" for a local one; `--json` reports `author` as `null`.
