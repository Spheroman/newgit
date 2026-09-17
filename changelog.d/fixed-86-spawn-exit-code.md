- `spawn` now exits non-zero when any resource fails to bind
  ([#86](https://github.com/Spheroman/newgit/issues/86)).

  A resource that printed `export: FAILED`, `render: FAILED`, `prepare:
  FAILED`, or `prepare: BLOCKED by ...` left `spawn` exiting 0 anyway, so a
  script, CI job, or agent driving newgit could not tell "instance ready"
  from "instance spawned but broken" without parsing the human-readable
  summary. `BLOCKED` counts alongside `FAILED`: a blocked `prepare` never
  ran, so that resource is no more usable than one whose `prepare` ran and
  failed. This mirrors #11's fix for `undo` ("a failed undo reports as
  Restored") and uses the same plain `exit 1` `undo` and `checkpoint
  --verify` already use, rather than a distinct code, so `if newgit spawn
  x; then ...` keeps working. The instance and its record are still
  created — exit 1 reports the state, it does not roll the spawn back —
  and the closing line now says so explicitly: `spawned with failures: 1
  of 4 resources did not bind.`
