- The `init_in_a_workspace_is_refused` test no longer flakes on Linux
  ([#204](https://github.com/Spheroman/newgit/issues/204)).

  It walked the configured workspace root two levels deep, as if a
  per-project directory sat between it and the workspace, and so listed the
  workspace's own contents and ran `newgit init` from whichever entry
  `read_dir` returned first. ext4 orders entries by filename hash, so a file
  came first on some runs and `current_dir` failed with `ENOTDIR`. The test
  now uses the workspace path spawn writes.
