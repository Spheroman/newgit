- `newgit init` in a clone of a project's GitHub mirror joins the project
  instead of quietly becoming a separate one
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A mirror carries `.newgit/config.toml` with `[remote]` in it, so `init`
  made a local project with its own root history and no mounted trackers
  while `newgit remote show` still read as linked. A member now ends where
  `newgit clone` ends, in place, and uncommitted changes or commits `main`
  does not contain stop it before anything moves. Not logged in, it says to
  log in. Anyone else gets a local copy that says it is not linked, in
  `init` and in `remote show`, and acts as a project with no remote, except
  that it will not export to the project's mirror. Projects with no
  `[remote]` are unchanged.
