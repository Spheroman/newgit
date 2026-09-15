- Definitions reject keys newgit does not recognize, instead of ignoring them
  ([#35](https://github.com/Spheroman/newgit/issues/35)).

  The serious case is a misspelling. `owneship = "branch"` used to parse
  cleanly and leave `ownership` at its default — and `ownership` is what
  decides whether per-branch teardown may touch the concrete resource, the
  difference between `newgit remove` stopping a dev server and it reaching a
  store shared with every other project on the machine. The same held for
  `merge_with_source`, `long_running`, `into_tracker`: every key whose
  safe-looking default is not what you meant.

  ```
  Error: could not parse TOML at .newgit/resources/app.toml

  Caused by:
      TOML parse error at line 1, column 1
        |
      1 | owneship = "branch"
        | ^^^^^^^^
      unknown field `owneship`, expected one of `ownership`, `depends_on`,
      `identity`, `workdir`, `ports`, `exports`, `render`, `actions`,
      `checkpoint`, `restore`, `cleanup`
  ```

  Leniency was a deliberate call when `kind` was dropped — a stale `kind =`
  line would simply be ignored, so no migration was needed. That reasoning was
  backwards: silently accepting a key the tool no longer understands *is* a
  backwards-compatibility affordance, and this project does not carry those.
  It also cost something immediately. Landing six issues as parallel branch
  instances, two of them kept writing `kind` into new test fixtures while the
  branch removing it was in flight; Git merged those without conflict, because
  new lines have nothing to conflict against, and everything compiled and
  passed. A grep caught it, not the tool.

  A definition carrying a key newgit dropped now fails to load. That is the
  intended outcome — the fix is deleting one line, and the error says which.

  Parse errors also stopped printing themselves twice: `could not parse TOML
  at <path>` interpolated the full toml snippet that anyhow then repeated as
  the cause.
