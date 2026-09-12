# newgit

`newgit` is a Rust CLI implementing the v1 MVP described in
[`newgit-v1-mvp.md`](newgit-v1-mvp.md). The central idea shows up in the code
shape: branch instances bind source state to user-defined trackers and
resources, instead of baking env files, installs, processes, databases, or
external resources into special internal lanes.

All seven v1 milestones are implemented. The workflow the MVP set out to make
feel normal now runs end to end:

```sh
newgit init
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit resource add deps --template pnpm
newgit resource add app --template process

newgit spawn auth-refactor
newgit action app.start auth-refactor
newgit checkpoint auth-refactor -m "before agent"
# an agent works in the branch workspace, and you dislike the result
newgit undo auth-refactor

newgit export auth-refactor --to ../public-export
newgit cleanup
```

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
  definitions and content lanes, resource definitions and lifecycle hooks,
  metadata storage, real-directory materialization, source tracker
  boundaries, checkpoints and undo, export, and cleanup.
- `crates/newgit-cli` exposes the CLI surface.
- `.newgit/` is created by `newgit init`. Committed: `config.toml`,
  `trackers/`, `resources/`. Gitignored local state: `branches/`,
  `snapshots/`, `checkpoints/`, `logs/`, `state/`, `local/`.

## Commands

Branch-instance lifecycle (top-level verbs; `[instance]` is inferred when run
inside a workspace):

- `newgit init` / `spawn <name>` / `status [name]` / `remove <name>`
- `newgit run [instance] -- <command>` — run with exports and ports loaded
- `newgit action <resource>.<action> [instance]`
- `newgit checkpoint [instance] [-m <msg>]` / `undo [instance] [--to <id>]` /
  `checkpoints [instance]`
- `newgit export [instance] --to <dir> [--include <path>] [--exclude <path>]`
- `newgit cleanup [--dry-run]`

Definition management (noun subcommands):

- `newgit tracker create <name> [--audience <a>] [--storage <s>]
  [--merge-with-source]`
- `newgit tracker track <tracker> <path>...`
- `newgit tracker capture|merge|pull|checkout <tracker> [instance]`
- `newgit tracker list`
- `newgit resource add <name> --template <template>` / `list` / `templates`

Resource templates: `process`, `pnpm`, `command-snapshot`, `external`.

## Two things worth knowing

**`export` fails closed.** Source ships (its audience is everyone) and so do
trackers whose audience is `public`. Anything narrower is withheld and
reported; `--include <path>` overrides. It writes one commit, not history,
because exporting the branch's commits would carry any file they contain —
including withheld ones. This is a path-level filter, not concealment.

**`cleanup` never breaks an undo.** It finalizes instances whose workspace is
gone, deletes unclaimed workspaces and dead process state, and prunes tracker
snapshot revs nothing references — but never a rev a checkpoint still points
at, and never a checkpoint record. `project`- and `user`-owned resources are
never torn down by per-branch cleanup.
