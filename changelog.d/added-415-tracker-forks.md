- `newgit tracker create <name> --from <tracker>` makes a fork: a tracker
  outside Git whose history starts at another's `main`, claiming the same
  paths and filling the same role, with `from = "<tracker>"` in its
  definition. `--role` names the role when the upstream has none yet
  ([#415](https://github.com/Spheroman/newgit/issues/415)).

  Members of a role (`local-db`, `staging-db`) had no shared history, so
  there was nothing to merge between them. Locally a fork is two refs in the
  one store, and on `newgit serve` a namespace in its upstream's store, so
  nothing is copied. A fork's audience can't be wider than its upstream's,
  and dropping or changing `from` doesn't lift that. The client refuses it at
  creation, in every command but `tracker list` and `tracker remove` (which
  show and fix it), and in any commit that changes definitions, merges
  included. `serve` records what each fork's repo was made from and serves
  the fork with that upstream's audience, whatever its definition says.
  Against newgit.dev's Worker only the client checks. `tracker list` shows
  what each fork came from. Only trackers outside Git can be forked yet. `tracker create`
  also takes `--role` for a plain tracker outside Git.
