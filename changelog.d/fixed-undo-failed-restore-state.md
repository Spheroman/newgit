- An undo whose restore fails leaves that resource and what depends on it
  down on purpose, and says how to retry. A failed database restore left
  `web` stopped with a restart error pointing at `newgit action db.serve`,
  and `status` suggested the same: starting the server over a restore
  that did not finish. Now the dependent's line says `not restarted, left
  blocked by db`, `status` says `failed to restore` with the retry (repair,
  then `newgit undo` again), `start` does not bring either back over it,
  and the next undo that succeeds restores both. A dependent's restore
  (a seed loading rows) also waits for a dependency the undo just started
  again, instead of running while it is still coming up. The reference
  says what is running when `[restore]` runs: its own process is stopped,
  its dependencies are back, its dependents are stopped.
