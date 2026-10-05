- `newgit env [instance]` prints the environment `newgit run` and every
  hook get, each variable labeled with the declaration it came from, and
  `newgit action <resource>.<action> --dry-run` prints the rendered command
  and the directory it would run in, running nothing
  ([#80](https://github.com/Spheroman/newgit/issues/80)).

  Before this, the only way to learn which database a `[restore]` would
  drop was to run it. Both read the same code a real run uses — `env` is
  the one assembly every hook gets, `--dry-run` shares the run's
  resolution and refusals — so neither can describe something other than
  what would happen. Values are printed verbatim, credentials included,
  as `newgit run -- env` would; the help text says so.
