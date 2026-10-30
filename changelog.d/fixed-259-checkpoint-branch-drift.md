- A checkpoint taken while the workspace is on another branch no longer
  moves the instance's branch to it, and undo no longer resets whichever
  branch happens to be checked out
  ([#259](https://github.com/Spheroman/newgit/issues/259)).

  Agents `git checkout -b try-x` inside a workspace all the time. A
  checkpoint used to treat `try-x`'s HEAD as the instance's source and
  silently move the instance's branch in the project repository to it, and
  `undo` then reset `try-x` to the instance's old commit. Now a checkpoint
  holds the workspace as it stands and records which branch was checked
  out (or that HEAD was detached). The instance's branch moves only when
  the workspace is on it, and the checkpoint says so when it is not,
  naming both branches. Undo puts HEAD back on the checkpoint's branch and
  moves only that one. `merge` repeats the warning, `status` shows
  ``feature-a@1a2b3c4d (workspace on `try-x`)``, and `checkpoints` marks
  the checkpoints taken off the branch. Refusing the checkpoint instead
  would have blocked `git push` and `undo` exactly while an agent is
  experimenting, which is when they matter most.
