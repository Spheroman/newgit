- `newgit login`, `logout`, and `whoami` sign in to a newgit remote with a
  GitHub account; `newgit remote add` creates the project there (pushing
  the store's history to its trunk) or joins it; `newgit members` manages
  access. In a linked project, `spawn` registers the instance — its own
  fork, and a token of its own kept in the workspace's `.git/` — and points
  the workspace's `origin` at trunk with newgit as Git's credential helper;
  `git push` publishes to the instance's fork on that token; `remove` takes
  the instance off the remote. A spawn whose registration fails still
  works locally and says so.
