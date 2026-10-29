- `newgit stop` and `newgit start` take down and bring back a resource that
  is not a process newgit supervises, through a `suspend` / `resume` action
  pair it defines ([#157](https://github.com/Spheroman/newgit/issues/157)).

  `stop` only signalled supervised processes, so a settled T3 Code thread
  whose `prepare` ran `supabase start` kept a dozen containers, nine ports
  and a gigabyte or two of memory until the thread was deleted. `stop` now
  runs `suspend` after a resource's dependents are down, and `start` runs
  `resume` before they come back; the `supabase` template defines the pair
  as `supabase stop` / `supabase start`, which keeps the data. `suspend`
  never runs for a `project` or `user` resource, which other instances
  share, nor for one that never came up. Each needs a command, and neither
  is accepted without the other.

  `start` no longer starts a resource whose dependency did not come back in
  the same `start`: it fails naming the dependency and stays owed, so a dev
  server is not brought up against a database that is still down.
