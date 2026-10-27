- The `supabase` template allocates and renders the pooler port, so a second
  instance's render is no longer refused
  ([#205](https://github.com/Spheroman/newgit/issues/205)).

  `supabase init` commits `[db.pooler] port = 54329` beside the six ports the
  template allocated, one number above a block of consecutive defaults. Once
  a first instance held those, the second instance's scan put `inbucket` on
  54329, and the render refused: the file would then name that port twice,
  which `newgit capture` could not reverse. A port the committed file names
  is a port the template has to own, so there is now a seventh `pooler`
  lane, rendered like the others. A project that added the template before
  this has its own copy under `.newgit/resources/`; add the `pooler` port
  and its `[[render]]` line there to get the same fix.
