- `newgit pull <tracker> [instance]` moves a mounted tracker's pin to its
  `follow` ref (`main` by default) in one commit, `Pull <tracker>: N commits
  from <ref>`, and `spawn` says when a mounted tracker is behind that ref
  ([#201](https://github.com/Spheroman/newgit/issues/201)).

  A child's `main` moves when an instance rooted at it merges or an upstream
  is synced, and the parent should not pick that up by surprise: nothing
  writes a commit on anyone's behalf. The pull is authored by whoever ran
  it, composes as a merge bringing the new commits in, and merges into
  `main` like any instance commit.
