- `spawn` and `checkpoint` on a project linked to a remote are several seconds
  faster.

  A spawn fetched every `branches = true` target's exported branch, which for
  a target rooted at a tracker the instance leaves out is that tracker's whole
  history: 6.6s of an 8.6s spawn here, for t3code. It now fetches it only when
  the instance binds that tracker. A checkpoint asked for a credential and
  pushed every tracker it syncs, changed or not, and asked for a fork it had
  already made; it now sends only what changed, and the remote reuses a
  credential it minted. A warm checkpoint went from 8.4s to 3.0s.
  `NEWGIT_TRACE=1` prints each call to the remote and how long it took.
