- A merge's lock lasts as long as the remote takes to check it
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  The remote checks a merge's index by running Git over every tracker it
  moves, and the first time a large history is there (Spheroman/t3code,
  365 MiB) that takes longer than the ten-minute merge lock. The lock ended
  while the check ran, and the finish was refused with "you hold no merge".
  The finish now extends the caller's lock for twenty minutes before the
  check, longer than the client waits.
