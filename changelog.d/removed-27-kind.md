- `kind` is gone from resource definitions
  ([#27](https://github.com/Spheroman/newgit/issues/27)). It was required on
  every resource and read nowhere — the only code that touched it was the
  parser, the struct, and the `KIND` column in `newgit resource list`. A
  required field with no behavior still reads as if it has one: a Supabase
  stack got `kind = "process"` because it runs a long-lived service, while
  two genuinely supervised resources in the same project also said
  `process` — one string, two meanings, and no rule to pick it by. Making
  `kind` real (defaults keyed off its value) was considered and rejected: that
  would let the field and the sections below it disagree, which is exactly
  how the Supabase resource got mislabelled. What a resource does was always
  fully described by its sections; `kind` never added anything to check
  against. A leftover `kind = "..."` line in an existing definition is
  silently ignored — the TOML parser already ignores unknown fields, so
  nothing had to change to make old files keep loading.

  `newgit resource list`'s `KIND` column is replaced with `PROFILE`, built
  from facts newgit can recompute from the TOML rather than a label someone
  wrote down once: `long-running` (a start action that doesn't exit),
  `ports`, `identity`, `render`, and `checkpoint:hash`/`command`/`external`.
  Unlike `kind`, this cannot drift from what the resource actually does,
  because it isn't stored anywhere to drift from.
