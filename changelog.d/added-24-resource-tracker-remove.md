- `newgit resource remove <name>` and `newgit tracker remove <name>` — the
  inverse of `resource add`/`tracker create` that only existed as `rm
  .newgit/resources/<name>.toml` before. Deletion by hand was predictable —
  definitions are plain TOML named by filename — but it was the one place in
  the tool where you had to reach around the CLI, and it turned up in the
  first session cleaning up a definition `resource add --template pnpm` had
  created on its own.

  Both refuse rather than leave damage behind, in the voice `newgit remove`
  already set: `resource remove` refuses if another resource still names it
  in `depends_on` (naming every dependent, and `--force` does not override
  this — a broken graph is never what you wanted) and refuses if a live
  instance still has it bound unless `--force` is given, in which case it
  drops the binding and releases the ports (there is no other ledger; a port
  is free the moment nothing claims it). `tracker remove` refuses the same
  way for a bound instance, refuses outright for `source` (Git/jj owns that
  history, there is no file to remove), and undoes exactly what `tracker
  track` did — the store's `.gitignore` block and each workspace's
  `.git/info/exclude` entries. Captured content under `.newgit/snapshots/`
  is left in place; it is now unreferenced, and a `newgit cleanup` bug fix
  alongside this (lane heads were pinned as roots forever, even after their
  tracker's definition was deleted) means that cleanup now actually reclaims
  it once nothing else — a checkpoint that captured it — still pins a rev.
