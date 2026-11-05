- Every push to the remote steps when it is too big, not only a merge's
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  `newgit remote add` seeds each tracker, a checkpoint sync sends its
  trackers, and an instance's fork gets its work, all with plain pushes,
  which did not step. Creating a project with Spheroman/t3code in it was
  refused at 359 MiB against Artifacts' 64 MiB. A plain push the remote
  refuses or drops now stages each commit it sends in steps, then pushes
  again.
