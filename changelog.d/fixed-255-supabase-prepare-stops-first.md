- The `supabase` template's `prepare` stops the stack before starting it
  ([#255](https://github.com/Spheroman/newgit/issues/255)).

  `supabase start` against a stack still running from an earlier binding
  exits 0 without re-reading the rendered ports, so the resource reported
  ready on ports nothing listened on. `supabase stop` keeps the volume, so
  the branch's data comes back on the new ports.
