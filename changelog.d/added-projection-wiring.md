- Work is split into its trackers when it leaves the machine. Before a
  spawn, the project root's new commits reach each code tracker's
  `instances/main`; a `git push` or remote checkpoint sync splits the
  instance's commits into `instances/<instance>`. Split commits are signed
  as their author when `commit.gpgSign` is set and the author is you.
  Code trackers without history (one defined by hand, or `project`) are
  carved the first time they are needed.
- `newgit log <tracker> [instance]` shows one tracker's history.
