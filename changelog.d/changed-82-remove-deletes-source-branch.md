- `newgit remove` deletes the instance's source branch when it holds
  nothing unique, and says why when it keeps it
  ([#82](https://github.com/Spheroman/newgit/issues/82)).

  It used to destroy the workspace and every resource it could tear down,
  then keep the one artifact that is cheap to keep and certain to bite: a
  leftover branch makes the next `spawn` of the same name adopt it instead
  of starting from `HEAD`, so every spawn/remove cycle needed a manual
  `git branch -D`. Now the branch goes when `spawn` created it, nothing has
  it checked out, and its tip is already on another branch, a tag, a
  remote-tracking ref, or the remote as of the last workspace `git push` —
  none of which can lose work. Otherwise it is kept and the output names the
  reason. `--keep-branch` and `--delete-branch` override; a forced delete
  prints the tip to recover from. Instances spawned before this change
  never recorded whether `spawn` created their branch, so `remove` treats
  their branch as adopted and keeps it.
