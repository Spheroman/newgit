- A fetch that deepens a store no longer fails when another Git rewrites
  the store's `shallow` file at the same moment
  ([#432](https://github.com/Spheroman/newgit/issues/432)).

  Git's own auto-maintenance, which it detaches after every fetch, runs a
  `git repack` that rewrites `shallow` in a shallow repository. A stepped
  fetch's next band could meet it and fail with `shallow file has changed
  since we read it`, or on finding `shallow.lock` taken. No lock of
  newgit's can cover Git's own processes, so such a fetch now waits and
  fetches again, as a fetch that finds a ref locked already did, and a
  stepped fetch no longer starts that repack between its own bands. A
  whole fetch never needed the stepped-fetch lock: Git touches `shallow`
  only for a fetch that deepens. The same holds for snapshot fetches and
  the `--unshallow` that brings a tracker's history.

  Tests that picked a free port by binding port 0 and letting go of it now
  read back the port they were given, or hold the listener: on a loaded
  machine something else took the port in between (#354).
