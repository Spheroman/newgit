- A dev server that crashes is no longer shown as `stopped` on a `ready`
  instance ([#261](https://github.com/Spheroman/newgit/issues/261)).

  Nothing waited on a supervised process, so one that exited, was
  OOM-killed or was `kill`ed looked exactly like one `newgit stop` took
  down, and an agent waiting for `ready` would test against a dead port.
  Each long-running action now runs under a small `sh` wrapper that waits
  on it and appends how it ended to its log (`[newgit] exited 1`,
  `[newgit] killed by SIGKILL`, or `[newgit] stopped by newgit`). `status`
  shows one that ended on its own as `exited(1)`, `killed(SIGKILL)` or
  `crashed`; an `autostart` action in that state makes the instance not
  ready, with `newgit action <resource>.<action>` as the retry; and
  `newgit start` restarts it along with what `newgit stop` stopped.
  `newgit cleanup` now retires a dead pid to `exited` rather than
  `stopped`.
