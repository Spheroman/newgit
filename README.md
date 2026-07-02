# newgit

`newgit` is a Rust CLI prototype for the v1 MVP described in
[`newgit-v1-mvp.md`](newgit-v1-mvp.md). The scaffold keeps the MVP's central
idea in the code shape: branch instances bind source state to user-defined
trackers, instead of baking env files, installs, processes, databases, or
external resources into special internal lanes.

## Tooling

This repository uses [`mise`](https://mise.jdx.dev/) to pin Rust.

```sh
mise install
mise run check
mise run test
mise run fmt
mise run clippy
```

You can also run the CLI through mise:

```sh
mise x -- cargo run -p newgit-cli -- init
mise x -- cargo run -p newgit-cli -- spawn auth-refactor
mise x -- cargo run -p newgit-cli -- status
mise x -- cargo run -p newgit-cli -- remove auth-refactor
```

## Workspace Layout

- `crates/newgit-core` holds the MVP domain model: branch instances, tracker
  definitions, bindings, metadata storage, real-directory materialization,
  resource templates, source tracker boundaries, and checkpoint records.
- `crates/newgit-cli` exposes the early CLI surface from the MVP.
- `.newgit/` is created by `newgit init` and is ignored by Git because it is
  local metadata.

The current skeleton implements the first usable slice:

- `newgit init`
- `newgit spawn <name>`
- `newgit status`
- `newgit remove <name>`
- `newgit tracker create <name>`
- `newgit tracker track <name> <path>...`
- `newgit tracker capture <name>`
- `newgit tracker merge <name>`
- `newgit tracker pull <name>`
- `newgit tracker checkout <name>`
- `newgit resource add <name> --template <template>`

The remaining MVP commands are present as explicit extension points so future
work can fill them in without changing the command vocabulary.
