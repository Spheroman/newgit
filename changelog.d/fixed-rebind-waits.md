- `newgit resource rebind` waits for what it started, as `spawn` and
  `start` do, and exits 1 when the instance it leaves is not ready.

  Its help already said it exited 1 when the resource did not come up, but
  it returned 0 at once, so an agent rebinding a server that never came up
  was told it had.
