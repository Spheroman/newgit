- The remote serves every project as an ordinary Git remote,
  `https://newgit.dev/<owner>/<name>.git`: `git clone` and `git pull` work
  with plain Git. Each branch is composed, in a per-project container running
  real `git` and `newgit`, from only the code trackers the caller may read,
  into a repository holding nothing else. Read-only; pushes go through a
  newgit workspace. The container sleeps when idle and restores from a
  snapshot.
