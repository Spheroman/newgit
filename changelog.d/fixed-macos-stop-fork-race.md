- `stop` on macOS no longer leaves a command running when the stop lands
  while a process in its group is forking.

  macOS lets a child still being forked join the process group just after a
  group signal goes out, so the child never gets it. Most often the child is
  the command itself, forked by newgit's wrapper right after a start: the
  wrapper died, the command lived on orphaned, and `stop` timed out with
  "ignored `term`". Removing the wrapper's trap (#288) made this rarer but
  did not close it. On macOS, `stop` now signals each process in the group
  and keeps sweeping until the group is gone, so anything that joins late
  is caught. Each process still gets the signal once. Linux was never
  affected and still uses a single `killpg`.
