- **`[cleanup]` no longer runs in the project root.** For an instance whose
  workspace was already gone, `newgit remove` and `newgit cleanup` ran its
  `[cleanup]` hook from the project root, so a hook written for the
  workspace, such as `rm -rf postgres-data`, deleted the project's own
  directory of that name. It now runs from an empty directory.
