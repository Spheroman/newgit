- A pre-undo checkpoint now says when its own contents are suspect, and no
  longer wastes a real capture command on a resource already known to be
  broken (#58).

  The message on a safety checkpoint — `state before undo to ckpt_001` —
  describes *when* it was taken, and a reader takes that as a description
  of what is in it. Those come apart precisely when the instance's last
  operation was an undo that did not finish: the workspace the next undo
  is about to capture is whatever that failed restore left behind, not a
  state anyone chose to be in. That checkpoint's message now says so:
  `state before undo to ckpt_001 (captured after an incomplete undo;
  contents may be partial)`.

  `newgit checkpoints`' `REASON` column already told a successful undo's
  safety checkpoint (`before-undo`) apart from a failed one's
  (`failed-undo`) and from a named one (`explicit`) — that distinction
  existed but was never covered by a test naming it directly, so a new
  test locks in the `(reason, undo_completed)` pair the column switches
  on.

  A safety checkpoint also no longer runs a resource's real checkpoint
  command — a `pg_dump`, say — against a resource whose own last restore
  already failed. That resource is known-broken, not merely unobserved,
  and spending the time to dump it produces the same rubble the message
  above now warns about, for a redo point nobody is likely to want. The
  skip is scoped to the automatic pre-undo path only: an explicit `newgit
  checkpoint` still runs the command, because a person asked for that one
  on purpose.
