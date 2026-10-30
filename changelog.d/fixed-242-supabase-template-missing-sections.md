- The `supabase` template says what to do about a `config.toml` that lacks
  a service's section, and offers a monorepo `workdir`
  ([#242](https://github.com/Spheroman/newgit/issues/242)).

  The Supabase CLI uses a built-in default port for a service with no
  section, so every instance shared it. A render `find` that matches 0 times
  now says so in its error and says to add the line rather than delete the
  rule. The template comments explain it, and a commented `workdir` notes
  that `[[render]] path` changes with it.
