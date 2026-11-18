- A process an undo could not start again reads `not started`, and the
  next start that succeeds clears it. The undo used to mark the resource's
  binding `failed`, which no start ever touched: a retried undo printed
  `restarted ok, left failed`, `status` called the instance not ready while
  it served, and suggested an action that was refused as already running.
  Only a rebind cleared it, and that moved a port. Now `status` says
  `` `web.serve` was not started — start it with `newgit start` ``, `start`
  brings it back, and the undo says the resource was restored but not
  running instead of calling it a failed restore command. Its recovery
  record names the action that really retries it (it said `.start`
  whatever the action was called).
