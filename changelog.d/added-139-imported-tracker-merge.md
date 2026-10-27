- A tracker whose history came from outside the project joins each branch
  through one merge commit instead of interleaving its history by date
  ([#139](https://github.com/Spheroman/newgit/issues/139)).

  Where it joined is a commit on the tracker's own history, a join marker
  (`compose::join` writes one), so it travels wherever tracker history goes.
  Joining appends exactly one commit and rewrites none: the project's commit
  ids stay the same everywhere, and `git log` and `git blame` under the
  tracker's paths reach through the merge into the imported history. A
  branch from before the import has no t3code until it takes the tracker,
  which gives it a join of its own on top of its commits. Writing the
  import's marker is #140; `newgit tracker use` taking a tracker is next.
