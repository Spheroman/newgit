- `newgit pr create`, `pr checkout`, and `pr list`, on `newgit serve`
  ([#167](https://github.com/Spheroman/newgit/issues/167),
  [#170](https://github.com/Spheroman/newgit/issues/170)).

  A reviewer can run a proposed change instead of only reading its diff:
  `newgit pr checkout 12` spawns the pull request's head with its
  uncommitted work, the trackers the reviewer may read, and its resources (a
  database restored from the checkpoint's dump, ports of this machine). A
  pull request names an instance, not a commit, so its head is the
  instance's latest push and follows each one; an `undo` or a `newgit
  checkpoint` never moves it. `pr create` takes its title and body from the
  last commit, and refuses unpushed commits, private instances, and a push
  with nothing new. The landing that puts a pull request's head on its
  branch marks it merged, in the same transaction, whoever merges: the
  author's instance or a reviewer's `pr-12`. newgit.dev's Worker has no pull
  requests yet, and the CLI says so.
