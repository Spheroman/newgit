- `resource list` and `tracker list` no longer drift once one row's value is
  long (#44).

  Both tables sized their columns from a hardcoded literal (`PROFILE` at 27,
  `AUDIENCE` at 9) rather than the row content actually being printed, so a
  resource with several profile traits, or a tracker whose audience string
  ran past the default width, pushed every column after it out of alignment
  for every row — not just its own. They now compute each column's width
  from every row plus the header, the way the branch-list table already
  does, so the table degrades to a wider column instead of to misalignment.

- A deposit-only tracker's bind line says `(deposit-only)` instead of
  `(0 files)` on `spawn`, `tracker pull`, and `undo` (#44).

  A tracker with no `paths` of its own exists only to receive `into_tracker`
  deposits, so it correctly has zero owned files to place in the workspace
  — but a real rev followed by `(0 files)` reads like a checkout that
  silently failed, right above the resources whose restore someone is
  usually there to debug. Naming the reason it's zero, rather than either
  printing the number or hiding it outright, is what actually removes the
  double-take: a bare omission still reads as a table with a value missing.
