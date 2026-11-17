- A resource's `[ready]` says what "up" means for it: a GET that answers
  200–399 (`http`), or a command that exits 0 (`command`), and how long to
  wait for it (`timeout_seconds`)
  ([#276](https://github.com/Spheroman/newgit/issues/276)).

  Up used to mean the ports of a running process answering, waited for
  five seconds whatever the server. A dev server that binds at once and
  serves 500s until it compiles was up too early, one that takes twenty
  seconds just read `starting`, and a Supabase stack a `prepare` left
  running had no readiness at all, so it read `ready` after it had gone
  down. The end of `spawn` now waits for the longest `timeout_seconds`
  among what is bound, a dependent's start waits for its dependency's
  check, and `status` shows how a failing check failed (`check` in
  `spawn --json`). With no `[ready]`, nothing changes.
