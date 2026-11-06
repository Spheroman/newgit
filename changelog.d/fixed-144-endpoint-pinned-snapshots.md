- `git fetch` from the Git endpoint no longer waits for every tracker's
  whole history ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A view needs a child's files at each pinned commit, not its history, so
  the container now fetches root's history and each pinned commit, one
  commit deep. Spheroman/t3code (365 MiB) made the first fetch after every
  deploy wait five minutes when it worked and over forty when Artifacts
  dropped it. Compose reads no child's parents anywhere, on the client too,
  so a reader of a child who may not read something that child mounts can
  see a commit's real message where they used to see `Update <tracker>`. A
  merge's checks, a fork's copy, and a mirror's pull still fetch whole
  histories, into a store of their own, in the background.
