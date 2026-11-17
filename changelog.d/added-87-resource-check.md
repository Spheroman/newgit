- `newgit resource check` finds the programs a definition's commands
  start that this machine does not have
  ([#87](https://github.com/Spheroman/newgit/issues/87)).

  A `[checkpoint]` calling `pg_dump` on a host with no Postgres client
  failed at the first checkpoint, and a `[restore]` calling `psql` only
  during the undo someone was relying on. The check reads the first word
  of every command in every action and hook (past `&&`, quotes,
  here-documents, assignments, and `exec`), looks it up on `PATH`, or in
  the project root for a relative path, and prints `ok` or `FAIL` for
  each with no instance and nothing run. Exits 1 when one is missing;
  `--json` for scripts.
