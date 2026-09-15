- `newgit status` reports when an instance's base has moved —
  `ok, main +2` — instead of staying silent while a rebase gets
  cheaper to do by the day
  ([#38](https://github.com/Spheroman/newgit/issues/38)).

  The binding record now keeps the branch `spawn --from` used (or
  whatever `HEAD` named when `--from` was omitted) alongside the
  revision it pointed at, fixed at spawn time and never touched by
  `checkpoint`. `status` compares that fixed point against the base
  branch's current tip in the store repo, computed fresh — never
  cached — on every call, so the count is exactly as current as the
  store's last fetch of upstream and never a stale answer dressed up
  as a fresh one. An instance spawned onto a branch that already
  existed has no recorded base and reports none, rather than guessing
  one; same when the base branch has since been deleted.
