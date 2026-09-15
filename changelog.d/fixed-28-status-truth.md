- `status` no longer reports states that are not true (#28), in three ways.

  A resource with an unrelated `long_running` action it never started — say,
  a stray `functions` action beside the `prepare` a Supabase resource
  actually uses — showed up as `stopped` on the strength of the declaration
  alone; the column now asks the supervisor whether *something it started*
  has since died, which is a fact newgit can actually know, rather than "does
  this resource's definition mention a long-running action at all." Telling
  those apart means a pid file has to survive being stopped — but leaving a
  raw pid sitting on disk indefinitely would reopen a worse lie once the OS
  recycles that number onto some unrelated process, so a stopped process now
  retires its file to the literal `stopped` instead of leaving the number
  behind, and `newgit cleanup`'s pid housekeeping does the same to a pid file
  whose process died on its own — rewriting it in place rather than deleting
  it, since deleting it is exactly what would have made `newgit cleanup`
  itself erase the fact `status` depends on.

  `blocked` used to be written into the binding record, which meant a
  resource with no `prepare` of its own — like an `admin`/`mobile` dev server
  waiting on a Supabase stack — stayed `blocked` forever once its dependency
  recovered, because nothing ever ran to recompute it. Blocked-ness is
  derived at read time from `depends_on` instead, so it clears the moment the
  blocker does, and `status` names what's blocking (`admin:blocked(supabase)`)
  rather than just saying so. A resource with no `prepare` at all goes
  further: it is `ready` from the moment it is bound, blocked dependency or
  not, because there was never a command to withhold from it in the first
  place — the same rule that makes a resource with a real `prepare` correctly
  stay `pending` while it waits.

  `status` is the command you run when you're already confused about what
  newgit thinks is true; it should not make that worse.
