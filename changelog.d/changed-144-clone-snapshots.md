- `newgit clone` fetches root's whole history and only the commits it pins
  of each mounted tracker, with that tracker's history fetched the first
  time something needs it
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Cloning a project with a large mount took 6.5 minutes, nearly all of it
  fetching a child's history nothing was going to read. The result is an
  ordinary project with the same commit ids as a full clone, and a commit
  under a mount splits, pushes, and merges with its pin as a parent. A real
  merge in a child, a rooted export, `newgit log <tracker>`, and a pull
  that does not fast-forward fetch the history, saying so once.
  `newgit clone --full` fetches every history up front. Projects cloned
  before this are unchanged.
