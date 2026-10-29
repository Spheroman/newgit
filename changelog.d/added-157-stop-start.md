- `newgit stop [instance]` and `newgit start [instance]` stop and start an
  instance's resources at once
  ([#157](https://github.com/Spheroman/newgit/issues/157)).

  The only lever was `newgit action <resource>.<action>`, one resource at a
  time, so a caller releasing an instance had to know every resource's
  actions and order them itself. `stop` stops every running long-running
  action, dependents first, and records what it stopped; `start` brings
  back exactly that, dependencies first, and nothing nobody had started.
  Both are idempotent and exit 0 with nothing to do, so T3 Code can call
  them on every settle and unsettle; `--json` says which resource failed,
  and exit 1 says that one did. Unlike `remove`, the workspace, trackers,
  installs, and port claims stay, so the instance comes back on the same
  ports. `remove` now stops running resources dependents first too.
