- A tracker is named by its path, read as a filesystem reads one
  ([#201](https://github.com/Spheroman/newgit/issues/201)).

  A child's name is local to its parent, as a file's is to its directory:
  `newgit tracker create repos --git --path apps/t3code/.repos` makes
  `t3code/repos`, and two parents can each have a `docs`. Commands take a path
  from where they run: at the top of a project that is `t3code/repos`, inside
  `apps/t3code` it is `repos`, `..` goes up, and a leading `/` starts from the
  top of the one namespace, `/remote/<owner>/<project>/t3code/repos`, or
  `/user/<project>/...` for a project on no remote. `tracker list` shows what
  is here. A name is never looked for further up, so it means one thing
  wherever it is typed. `requires` in a nested definition, and `root` and
  `trackers` in an export target, take paths too. `__` is reserved in a
  tracker's name, and a name cannot start or end with `_`.
