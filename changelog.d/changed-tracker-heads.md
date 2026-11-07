- The remote keeps each tracker's head as its own record, and logs every move.

  A branch's list was one value, rewritten whole by each merge. It is now built
  from a head per tracker, and every head that moves is logged with who moved
  it and when. Nothing a client sees changes: this is the first step of landing
  at anchors (`docs/design/anchoring.md`). Branches from before are adopted
  from their lists the first time they are read.
