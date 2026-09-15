- `resource add --template` says why its extra output exists and how to
  undo it, and prints paths relative to the project instead of absolute
  ([#23](https://github.com/Spheroman/newgit/issues/23)).

  `--template pnpm` on a project that already uses npm creates
  `pnpm-store.toml` alongside `deps.toml` because `pnpm` depends on it — debris
  the user has to notice on their own, since `depends_on` is a graph edge, not
  a file on disk. `--template command-snapshot` creates a *tracker*, not
  another resource, because `into_tracker` needs somewhere to deposit; that
  asymmetry was buried behind identical-looking `companion:`/`tracker:`
  prefixes. The line now leads with the created thing's name and kind, names
  the key that pulled it in, and says it can be edited or deleted — keeping
  the path, since "delete the file" is only actionable if it says which file:

  ```
  Added resource `deps` from `pnpm` at .newgit/resources/deps.toml
    also created resource `pnpm-store` at .newgit/resources/pnpm-store.toml
      required by deps.depends_on — edit it, or delete the file if this project doesn't need it
  ```

  Someone who ran `newgit tracker create db-snapshots` by hand and hit
  `already exists at ...` had no way to know a `resource add --template
  command-snapshot` had created it — the error read like a bug in their own
  script. It now says so in one clause.
