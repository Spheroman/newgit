- `newgit resource check` finds the programs a definition's commands
  start that this machine does not have
  ([#87](https://github.com/Spheroman/newgit/issues/87)).

  A `[checkpoint]` calling `pg_dump` on a host with no Postgres client
  failed at the first checkpoint, and a `[restore]` calling `psql` only
  during the undo someone was relying on. The check reads every action and
  hook command as shell (`sh -n` must accept it; a real parser walks it),
  looks up each program the shell would run (inside `if`, loops, `case`
  arms, groups, functions, `$(...)`, and past `env`, `exec`, `timeout`,
  `xargs`, `sh -c`, `eval`) on `PATH`, or, for a path, from where the
  command's `workdir` and any `cd` put it, and requires it to be
  executable. Each is `ok`, `FAIL`, or `skip` with the reason it cannot be
  known before an instance runs (a template or variable names it, a `cd`
  it cannot follow, a `command -v` guard, an ignored path a build makes).
  Exits 1 only on a `FAIL`; `--json` for scripts.
