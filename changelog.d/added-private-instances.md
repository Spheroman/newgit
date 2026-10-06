- `newgit spawn <name> --private`: on the remote, only the instance's owner
  sees it — its record, checkpoints, and forks of the code trackers — until
  `newgit merge` publishes its work. Everyone else, the project's owner
  included, gets the same answer as for an instance that does not exist.
  Refused without a newgit remote, where a `git push` would publish it.
  `newgit status` shows `private`.
