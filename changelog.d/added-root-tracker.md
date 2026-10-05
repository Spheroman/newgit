- `newgit init` creates the `root` tracker, which owns everything no other
  tracker claims and starts with the project's own Git history (borrowed,
  not copied). Trackers may now nest — `apps` can hold `apps/electron` for
  another tracker — and a path belongs to its most specific owner, so
  capturing or restoring the outer tracker leaves the inner one's files
  alone. Two trackers claiming the identical path is still an error.
  Tracker definitions gain `secret` (default `true`: kept out of Git and
  captured from files, as every tracker has been); `newgit tracker list`
  shows root first.
