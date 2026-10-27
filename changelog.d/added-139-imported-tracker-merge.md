- A tracker whose history came from outside the project joins each branch
  through one merge commit instead of interleaving its history by date
  ([#139](https://github.com/Spheroman/newgit/issues/139)).

  Where it joined is a commit on the tracker's own history, a join marker
  (`compose::join` writes one), so it travels wherever tracker history goes.
  Joining appends exactly one commit and rewrites none: the project's commit
  ids stay the same everywhere, and `git log` and `git blame` under the
  tracker's paths reach through the merge into the imported history. A
  branch from before the import has no t3code until it takes the tracker,
  which gives it a join of its own on top of its commits. Later commits
  from outside (an upstream sync) come in the same way, as one merge
  marked `Newgit-Follows: <view commit>`, so their old dates never place
  them under newer work.

  Such a tracker is stored relative to its own root: its definition's new
  `mount` key says where that root sits in the workspace (`mount =
  "apps/t3code"`, `paths = ["."]`), so its commits stay the outside
  repository's own. Views show its history under the mount, as a copy
  every machine makes identically, and split commits work there back at
  the tracker's root. Writing the import's marker is #140, pulling is
  #142; `newgit tracker use` taking a tracker is next.
