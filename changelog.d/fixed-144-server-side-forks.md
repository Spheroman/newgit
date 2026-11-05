- An instance's fork of a large tracker is made by newgit.dev itself, and a
  sync waits for it
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Artifacts' own fork of Spheroman/t3code (365 MiB, 8,164 commits) never
  finished: it said `FORK_IN_PROGRESS` for hours, so the first push from an
  instance that changed it never landed. The remote no longer uses it. It
  makes the fork as an empty repo and has its Git endpoint's container copy
  the tracker's branches in, with the fetch and push that already step
  through large histories (`newgit endpoint-copy`). The fork route answers
  `202 {"forking": true}` while the copy runs, and the client asks again,
  saying once a minute that it is still forking. A copy that dies part way
  is carried on the next time, never handed out half done; one that fails
  answers 502 with Git's or Artifacts' own error. A fork left stuck by
  Artifacts' fork is deleted and made again.

  Every fork takes this path, so a small tracker's first fork now waits
  for the endpoint's container to wake and for at least one 5-second poll,
  where Artifacts' fork took 5–22 s.
