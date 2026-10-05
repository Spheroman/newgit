- A checkpoint records which trackers the instance did not bind, and `newgit
  undo` restores that too: trackers dropped since come back with their
  content, trackers used since are dropped again, and the resources they
  bring start or stop to match. `spawn --from-checkpoint` binds what the
  author's checkpoint bound.
