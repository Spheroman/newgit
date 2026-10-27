- `spawn --from-checkpoint` now exits 1 when the instance it leaves is not
  ready, and says why: each restored resource's line carries the restore's
  verdict (`ok` or `FAILED`), as `undo` prints it, and a failed restore's
  recovery record is named on stderr
  ([#208](https://github.com/Spheroman/newgit/issues/208)).

  #86 and #124 gave the plain `spawn` and both `--json` forms an exit code
  that says what the summary says, and left the human-readable
  `--from-checkpoint` behind: it printed `resource: seed command` and exited
  0 whether the command had succeeded or not, so the only place the failure
  showed was a later `newgit status`. It now judges by the same condition
  `--json` does: a failed restore, or a resource the checkpoint held no
  state for that failed to bind, both exit 1. The headline names what did
  not come up, and every resource the instance has gets a line — one the
  restore did not touch shows its bind's `prepare` verdict, as a plain
  `spawn` prints it, and so does one whose restore ran nothing (no
  `[restore]`, `none`, `external`) and left it as a failed bind had it. The
  instance and its record are kept, as before.
