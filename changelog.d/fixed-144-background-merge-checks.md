- A merge whose check takes many minutes on the remote finishes
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  The remote checks a merge by running Git over every tracker it moves, and
  the first time a large history is there (Spheroman/t3code, 365 MiB) that
  took longer than the fifteen minutes `newgit merge` waited for an answer,
  so every finish timed out. The remote now runs its checks in the
  background and answers `202 {"checking": true}` until they end; `newgit
  merge` asks again every ten seconds, says once a minute that it is still
  waiting, and gives up after an hour. The merge's lock holds while it asks,
  and the check's read passwords last as long as the check may run.

  A fetch the remote drops is now tried whole twice more before newgit
  fetches it in steps, and the steps start at 2048 commits rather than 256:
  for t3code, five minutes whole against over half an hour in small steps.
