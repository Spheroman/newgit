- `newgit tracker create <name> --from <tracker>` makes a fork: a tracker
  outside Git whose history starts at another's `main`, claiming the same
  paths and filling the same role, with `from = "<tracker>"` in its
  definition. `--role` names the role when the upstream has none yet
  ([#415](https://github.com/Spheroman/newgit/issues/415)).

  Members of a role (`local-db`, `staging-db`) had no shared history, so
  there was nothing to merge between them. Locally a fork is two refs in the
  one store, and on `newgit serve` a namespace in its upstream's store, so
  nothing is copied. A fork is never readable by anyone its upstream is not:
  a wider audience is refused at creation, on every load, and in any commit
  that changes definitions, and `serve` reads a wider one with its upstream's
  audience. Only trackers outside Git can be forked yet. `tracker create`
  also takes `--role` for a plain tracker outside Git.
