- `newgit render --check` resolves every `[[render]]` against the working
  tree and reports, per file, which `find` matched and which did not — no
  instance, no spawn, no commit
  ([#56](https://github.com/Spheroman/newgit/issues/56)).

  A render's input is committed content, and that has to stay true: it is
  what makes `undo`, `tracker pull`, and re-renders idempotent. But it means
  the `find` strings you just wrote while adopting a `[[render]]` are
  invisible to the tool that would validate them until you commit — the
  adoption loop was edit, commit, spawn, read the failure, edit again.
  `render --check` reads the working tree instead, so that loop is edit,
  check, edit. It doubles as a drift detector outside adoption too: run it
  in CI and a default that moved upstream fails the build instead of the
  next `spawn`. Exits non-zero and names the file and the string on any
  mismatch.
