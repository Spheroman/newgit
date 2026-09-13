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
newgit tracker capture runtime-env --from-store   # seed the lane from the file you already have
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

## Install

Prebuilt binaries for macOS and Linux (arm64 and x86_64) are attached to each
[release](https://github.com/Spheroman/newgit/releases). Download, verify, and
put `newgit` on your `PATH`:

```sh
TARGET=aarch64-apple-darwin   # or x86_64-apple-darwin, {x86_64,aarch64}-unknown-linux-gnu
VERSION=v0.1.0
BASE=https://github.com/Spheroman/newgit/releases/download/$VERSION
curl -fsSLO $BASE/newgit-$TARGET.tar.gz -O $BASE/newgit-$TARGET.tar.gz.sha256
shasum -a 256 -c newgit-$TARGET.tar.gz.sha256
tar -xzf newgit-$TARGET.tar.gz
install -m755 newgit-$TARGET/newgit ~/.local/bin/newgit
```

From source, which needs the Rust toolchain in `.mise.toml`:

```sh
cargo install newgit --locked
```

Requirements: a `git` binary on `PATH` (newgit drives source through Git's
public interface rather than reimplementing it) and a Unix-like OS — process
supervision uses process groups and signals, so Windows is not supported.

Check what you installed. The commit is part of the version because `0.x`
moves fast, and `-dirty` means the binary does not match any commit:

```sh
newgit --version   # newgit 0.1.0 (15e4d0da0e51)
```

## Adopting it in a project

```sh
cd your-project        # any Git repository with at least one commit
newgit init
```

`init` prints what to commit. The split matters: `.newgit/config.toml`,
`trackers/`, and `resources/` are the control plane and belong in Git, so
teammates and CI see the same orchestration. Everything else under `.newgit/`
— branch bindings, captured content, checkpoints, logs, runtime state — is
local and is gitignored for you.

Nothing about your repository changes until you ask for it. `init` writes
`.newgit/`, and `tracker track` appends to `.gitignore`; no command rewrites
source history, and `remove`/`cleanup` never touch the store repository's
branches.

## Development

This repository uses [`mise`](https://mise.jdx.dev/) to pin Rust.

```sh
mise install
mise run ci        # fmt-check + clippy -D warnings + test, exactly what CI runs
mise run test
mise run install   # build and install into ~/.cargo/bin
```

CI runs the suite on Linux and macOS, checks the declared MSRV, and smoke
tests the release binary against a throwaway project — the tests here drive
real `git`, real process groups, and real ports, so they are integration
tests by nature.

## Workspace Layout

- `crates/newgit-core` holds the MVP domain model: branch instances, tracker
  definitions and content lanes, resource definitions and lifecycle hooks,
  metadata storage, real-directory materialization, source tracker
  boundaries, checkpoints and undo, export, and cleanup.
- `crates/newgit` is the CLI, published as the `newgit` crate.
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
- `newgit tracker capture <tracker> --from-store` — seed a lane from the store
  repo's working tree, so the first `spawn` comes up with the content
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
