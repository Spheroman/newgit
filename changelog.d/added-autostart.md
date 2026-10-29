- `autostart = true` on a long-running action has `newgit spawn` start it
  once every `prepare` has run, dependencies first, so a spawned instance
  comes up with its dev server running. The `process` template sets it.

  Before, spawn ran only `prepare`, and a dev server waited for someone to
  run `newgit action app.start`; T3 Code and Claude Code never did. A start
  that fails is reported like a failed `prepare`: the spawn says which
  resource did not come up and exits 1, and a resource that depends on it
  is not started against it. `spawn --json` lists each attempt under
  `started`, in the shape `newgit start --json` uses. `spawn
  --from-checkpoint` starts them after its restore, and `newgit tracker use`
  starts those of the resources a tracker brings, which `drop` stops. An
  existing resource definition is unchanged until you add the key.
