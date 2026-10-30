- A rendered file stays out of `git status`, `git add -A` and checkpoints
  while a code tracker is dropped.

  newgit keeps an instance's values out of Git by marking each rendered
  file `--skip-worktree`. Dropping a code tracker that owns paths makes
  the workspace a sparse checkout, and a sparse checkout decides that bit
  itself: `sparse-checkout set` and `disable` work every file's bit out
  again from the patterns, Git commands that update the working tree
  (`reset --hard`, `stash`, `checkout`) do too, and reading the index
  cleared the bit of every marked file Git found on disk. So once `tracker
  drop` or `spawn --drop` had dropped one, and still after the `tracker
  use` that bound it again, `git status` showed the rendered file as
  modified, an agent's `git add -A && git commit` committed this
  instance's port, and a checkpoint held it as uncommitted work: `newgit
  checkpoint` printed `+ uncommitted changes` with nothing else changed,
  and an undo to that checkpoint printed `+ uncommitted changes
  reapplied`. An agent's `git reset --hard` put the committed value back
  on disk, and `git rebase` refused over unstaged changes. And while one
  code tracker was dropped, `tracker drop` refused another whose paths
  held a rendered file, as uncommitted work.

  The sparse patterns now leave the rendered files out too, the workspace
  sets `sparse.expectFilesOutsideOfPatterns` so that Git keeps the bit of
  such a file, and a checkpoint sets it again for its own `git add -A`. An
  instance whose code tracker an earlier newgit dropped gets the patterns
  and the setting at its next `tracker drop` or `tracker use` of a code
  tracker, or the next undo (without `--only`), `tracker checkout` or
  `tracker pull`, which render again. Until then `git status` there still
  shows the rendered file, but its checkpoints no longer hold it.

  The setting also keeps the bit on a dropped tracker's file that is
  written into the working tree again. `git status` no longer shows an
  edit to it, and `tracker drop` of a tracker whose paths hold it no
  longer refuses over it. A checkpoint still names it and `undo` still
  refuses over it, since they look past that bit, and `tracker use`
  brings it into view as a modification.
