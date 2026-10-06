- A commit that changes code trackers with different audiences is refused
  where it would leave the machine (push, sync, merge): every tracker it
  changes keeps a copy of its message, so a message about work only some
  may read would land where everyone else reads it. Make one commit per
  audience. Merge commits are exempt.
