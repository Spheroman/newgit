- `newgit pr create`, `pr checkout`, and `pr list`, on `newgit serve`
  ([#167](https://github.com/Spheroman/newgit/issues/167),
  [#170](https://github.com/Spheroman/newgit/issues/170)).

  A reviewer can run a proposed change instead of only reading its diff:
  `newgit pr checkout 12` spawns the pull request's head with its
  uncommitted work, the trackers the reviewer may read, and its resources (a
  database restored from the checkpoint's dump, ports of this machine). A
  pull request names an instance, not a commit, so its head is the
  instance's latest checkpoint and follows each push. `pr create` takes its
  title and body from the last commit, and refuses unpushed commits and
  private instances. `newgit merge` marks the pull request merged in the
  same transaction that lands it. newgit.dev's Worker has no pull requests
  yet, and the CLI says so.
