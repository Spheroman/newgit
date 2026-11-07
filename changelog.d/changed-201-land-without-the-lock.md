- Merges and webhook pulls land without a lock
  ([#201](https://github.com/Spheroman/newgit/issues/201)). A merge pushes
  its commits to landing refs (`refs/newgit/landing/<commit>`) that nothing
  reads. The remote then moves the branch's list in one compare-and-swap,
  and moves the repos' branches itself afterwards with a ref-only push. Two
  merges can now run side by side: whichever lands second is told `main`
  moved and merges again. The ten-minute merge lock is gone. A merge that
  stopped before its finish no longer leaves root's ref ahead of the list.
  If a child's `main` can't catch up, `merge` says so as a warning, and the
  merge still stands.
