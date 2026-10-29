- The `process` template says its port is optional
  ([#244](https://github.com/Spheroman/newgit/issues/244)).

  A worker, watcher, or `supabase functions serve` has no port of its own.
  The template comments now say to delete `[ports]`, `[exports]` and
  `[[render]]` for those, instead of leaving you to work it out.
