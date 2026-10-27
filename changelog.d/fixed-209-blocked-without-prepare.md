- A dependent with no `prepare` is reported blocked by every surface when
  its dependency fails, `spawn --json` included
  ([#209](https://github.com/Spheroman/newgit/issues/209)).

  Such a resource is recorded `ready` on purpose (#28: nothing was withheld
  from it, and nothing would ever rerun to move it off `pending`), and
  `status`, `action` and the human-readable `spawn` derive blocked-ness
  from the graph at read time instead. `spawn --json` read the record's
  `ready` straight through with an empty `blocked_by`, so a script saw the
  resource ready while `status` said `blocked(base)` and `action` refused
  to start it. It now derives `blocked` and `blocked_by` the same way the
  other surfaces do. A script that treated `ready` with a failed
  dependency as fine will now see `blocked`.

  The human-readable `spawn` also said `prepare: BLOCKED by base` for a
  resource with no `prepare`, then counted only the failed dependency in
  "1 of 2 resources did not bind". It now says `BLOCKED by base (no prepare
  to run; its actions are refused until that is repaired)`, and the count
  includes every blocked resource, as the spec already said it should.

  A resource that failed itself (its export, render or `prepare`) with a
  dependency down too is now `failed` in `status`, as it already was in
  `spawn` and `spawn --json`, rather than `blocked(...)`: repairing the
  dependency would not fix it.
