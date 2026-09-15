- `[exports]` may compose a dependency's exports
  ([#40](https://github.com/Spheroman/newgit/issues/40)).

  A `[[render]]` could already see every export bound so far, so a *file*
  could carry another resource's URL while the resource itself could not
  publish one — the wrong way round, since `[exports]` is what produces those
  values in the first place. Bindings happen in dependency order, so the
  values were already there; nothing passed them.

  ```toml
  depends_on = ["supabase"]

  [exports]
  SUPABASE_FUNCTIONS_URL = "{{exports.SUPABASE_API_URL}}/functions/v1"
  ```

  This is the expressible form of what #40 tried to write as
  `{{ports.supabase.api}}`: there is no syntax for another resource's ports,
  and now there does not need to be.
