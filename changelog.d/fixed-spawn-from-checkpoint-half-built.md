- A `newgit spawn --from-checkpoint` that fails after the instance is made
  says how to finish it or remove it.

  It exited 1 with one error line and left the workspace, the record, and
  a prepared database behind, so running it again was refused with no
  word of why. The instance is still kept for inspection, as a plain
  `spawn` that fails midway keeps it, and the error now says so and names
  `newgit undo <name> --to <id>`, which restores it to the checkpoint once
  what failed is fixed, and `newgit remove <name> --purge`, which removes
  it so the spawn can run again. A spawn whose restore failed (exit 1 with
  the full report) names the same two commands on a `retry:` line, since
  the `prepare` the readiness advice suggests would start the resource
  empty.
