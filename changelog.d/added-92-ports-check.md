- Add `newgit ports --check`: detect a host port that is listening but that
  no binding record claims
  ([#92](https://github.com/Spheroman/newgit/issues/92)).

  The inverse of `render --check`: it runs `lsof -nP -iTCP -sTCP:LISTEN`
  and compares what is actually listening against every binding record,
  flagging a listening port inside any resource's allocatable range that
  nothing claims. It deliberately does not fix the allocator's own bind
  probe (`bindable` still only tries `127.0.0.1`, the root cause of #93)
  — it is a cheap, inspectable mitigation that would have caught #93's
  port collision after the fact, without requiring the probe to get
  Docker-published ports right.

  Attribution to an instance is honest rather than complete: a listening
  port is named to an instance only when that instance's name or
  workspace path genuinely appears in the owning process's command line.
  On macOS, a Docker Desktop container's published port is fronted by
  Docker's own VM proxy, whose command line says nothing about the
  instance that owns the container, so most Docker-caused conflicts come
  back unattributed rather than guessed — an honest "something claims
  this port and it isn't newgit" is still the useful finding.

  Bare `newgit ports` (no `--check`) lists every instance's claimed ports
  straight from its binding record — no probing, no `lsof` dependency —
  since that ledger is useful on its own and costs nothing to print.

  If `lsof` is missing or exits with anything other than "nothing
  found", `--check` says it could not check rather than silently
  reporting a clean pass.
