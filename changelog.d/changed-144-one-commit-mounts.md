- A mounted tracker's history stays in its own repo: a mount joins the view
  as one ordinary commit, and each `newgit pull` as one more
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Importing t3code put 8,164 of its commits into newgit's history and onto
  the public mirror, through a `git subtree`-shaped merge with the child's
  commits replayed under the mount. No view reaches a child's commits now,
  so `git log`, `git blame`, and `git bisect` under a mount stop at the
  import and at each pull; the child's own repository has the rest.
