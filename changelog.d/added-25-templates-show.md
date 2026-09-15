- `newgit resource templates --show <name>` prints one starter template's
  TOML in full ([#25](https://github.com/Spheroman/newgit/issues/25)).

  The one-line descriptions in `newgit resource templates` can't say which
  sections a template carries, whether it declares `[identity]`, or what
  `checkpoint.mode` it picks — and the template bodies are written as
  documentation in their own right (the `process` template's comment on why
  `[[render]]`'s `find` is literal and must match exactly once is a small
  tutorial on the feature). The only way to read one used to be
  create-cat-delete: `newgit resource add`, `cat` the file, `rm` it, times
  four, in a repo that had nothing else in it yet. `--show` prints the same
  `&'static str` the binary already carries for `add`, so there is one copy
  of the text and it can't drift — needs no `.newgit/` and creates nothing.

  A template that brings a companion resource or tracker along (`pnpm` →
  the `pnpm-store` resource it depends on, `command-snapshot` → the
  `db-snapshots` lane it deposits into) says so above the TOML, as `#`
  comments naming which kind each companion is — a resource and a tracker
  are instantiated differently, and confusing the two was the seed of
  [#23](https://github.com/Spheroman/newgit/issues/23). Comments keep the
  whole output pasteable straight into a `.newgit/resources/<name>.toml`.

  `definitions.md`'s `[captures]` section quoted the `external` template's
  `prepare` action verbatim; it now points at `--show external` instead, so
  that example can't drift from the template either.
