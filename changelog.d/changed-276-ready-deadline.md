- A `[ready]` check still failing once its `timeout_seconds` has passed is
  a problem, not `starting`: `spawn` and `newgit start` exit 1 and `status`
  says `not ready`, naming the check and what it answered
  ([#276](https://github.com/Spheroman/newgit/issues/276)).

  A resource newgit does not supervise (a Supabase stack a `prepare` left
  running) read `starting` forever with `spawn` exiting 0, though with no
  process to watch, still starting and gone down look the same. The
  timeout is now a deadline wherever a definition declares `[ready]`,
  counted from the process's start, or, with none, from the last
  `prepare`, `resume` or `restore`, so `status` gives the answer `spawn`
  did. A supervised process with no `[ready]` has no deadline and stays
  `starting`. `newgit start` now waits for what it brought back, as `spawn`
  does, and adds `starting` and `problems` to its JSON when there are any;
  `spawn` exits 1 whenever the instance it leaves is not ready, and
  `spawn --json` has `problems`. Declared ports of an unsupervised
  resource are still not checked: they are reservations.
