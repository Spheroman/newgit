- A tracker whose history merges in a large upstream copies into an
  instance's fork on the remote. Stepping a push past Artifacts' 64 MiB
  limit went along the first-parent line and never split its last
  commit, so a merge that brought more than 64 MiB was one push it could
  not make: copying `t3code` (a "merge upstream" of 1,626 commits) failed
  after 23 minutes. A merge too big for one push now has its other
  parents' history sent first, in steps of its own, and the tip is staged
  like every other commit.
