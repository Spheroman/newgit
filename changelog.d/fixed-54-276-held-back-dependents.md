- **Held-back dependents, after review of #54 and #276.** `newgit status`
  showed an instance as `ready` when a spawn had held an `autostart` action
  back behind a dependency that was not up; it now shows the resource as
  `not started` and the instance as not ready. A dependency that did not
  come up is waited for once per command, not once for each of its
  dependents. A dependency's `[ready]` check is run when its process is
  started by hand, not only when newgit starts it. `undo` and `spawn
  --from-checkpoint` wait for a dependency to answer before restarting what
  depends on it, as spawn does. A `[ready] http` check is no longer sent
  through `HTTP_PROXY` or `ALL_PROXY`, and a `[ready] command` that runs
  out of time is killed with whatever it started.
- **`left behind:` after review of #83.** The `newgit tracker track`
  command a spawn prints quotes a path with a space in it; a file some
  tracker already owns is no longer named as carried by none; `.dev.vars`
  and `.npmrc` are named too.
