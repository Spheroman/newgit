- `newgit tracker use <tracker>` and `newgit tracker drop <tracker>` change
  what an instance binds. An instance's history holds every code tracker its
  user may read; dropping one takes its files out of the working tree (Git's
  sparse checkout) and its work out of the instance's scope — nothing splits
  into it, and a merge leaves it at the branch's commit — without a commit
  or any change to history. A drop is refused while the tracker has
  uncommitted work, unless `--stash` (brought back by `use`) or `--discard`
  (after a checkpoint, so `newgit undo` gets it back). Dropping a secret
  tracker captures its content first; `use` projects it back. `root`,
  `project`, and `access` cannot be dropped.
