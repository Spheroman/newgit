- `newgit export` now commits every path it ships, including `--include`d
  and public tracker paths
  ([#94](https://github.com/Spheroman/newgit/issues/94)).

  `tracker track` gitignores a lane's paths, and that `.gitignore` ships
  with the source tree, so the export's `git add -A` silently left every
  tracker file on disk and out of the repository. `git status` showed a
  clean tree and a clone of the export did not have the file, so the check
  the command recommends could not see what it had done. The export's
  commit is now exactly the plan, and the output lists any committed file
  the shipped `.gitignore` still matches — Git tracks it regardless, but a
  new file beside it would be ignored. The `.gitignore` itself is not
  rewritten: it is source, and it is what keeps a recipient's own copy of a
  withheld path out of their commits.
