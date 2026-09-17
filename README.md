# newgit

`newgit` is a Rust CLI implementing the v1 MVP described in
[`newgit-v1-mvp.md`](https://github.com/Spheroman/newgit/blob/main/newgit-v1-mvp.md)
— that design document and
[`newgit-architecture.md`](https://github.com/Spheroman/newgit/blob/main/newgit-architecture.md)
live on GitHub, not in the published crate. The definition format travels
with the binary instead: `newgit reference` prints every tracker and resource
key, its default, and which template variables each hook sees. The central
idea shows up in the code
shape: branch instances bind source state to user-defined trackers and
resources, instead of baking env files, installs, processes, databases, or
external resources into special internal lanes.

All eight v1 milestones are implemented. The workflow the MVP set out to make
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
VERSION=v0.2.0
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
newgit --version   # newgit 0.2.0 (15e4d0da0e51)
```

## Adopting it in a project

```sh
cd your-project        # any Git repository with at least one commit
newgit init
```

`init` prints what to commit. The split matters: `.newgit/config.toml`,
`trackers/`, `resources/`, and `scripts/` are the control plane and belong in
Git, so teammates and CI see the same orchestration. Everything else under
`.newgit/` — branch bindings, captured content, checkpoints, logs, runtime
state — is local and is gitignored for you.

Scripts your resources shell out to go in `.newgit/scripts/` and are
referenced as `{{scripts}}/<name>`. They resolve from the store, not from the
workspace, which is the same rule the resource definitions follow — so you can
edit a `prepare` script and re-run `newgit action` without committing the
attempt first. A script your *project* owns still lives in the project tree
and must be committed before a spawn that calls it.

Nothing about your repository changes until you ask for it. `init` writes
`.newgit/`, and `tracker track` appends to `.gitignore`; no command rewrites
source history, and `remove`/`cleanup` never touch the store repository's
branches.

### What your package manager will cost you

Every branch instance gets its own installed dependencies. That is what makes
instances independent — two branches with different lockfiles must not share
a dependency tree, or one branch's install silently rewrites the other's —
and it is why installs are resources rather than trackers.

How much that costs is set by your package manager, not by newgit. **pnpm**
hardlinks packages from one shared store into each tree, so a tenth instance
adds directory entries rather than gigabytes; Yarn PnP skips the tree
entirely, and `uv` does the same for Python. **npm** expands a full copy per
instance — its cache holds tarballs, so `npm ci` re-expands every time, and
ten instances of a monorepo means ten full copies of `node_modules`. `pip`
into a per-instance venv behaves the same way.

This is the one place where newgit multiplies a cost you already had instead
of absorbing it, and there is no lever on newgit's side: a shared installed
tree is the thing that would be wrong. If you run many instances of a large
repository, a package manager with a content-addressed store is worth more
here than it is on plain Git.

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
  `trackers/`, `resources/`, `scripts/`. Gitignored local state: `branches/`,
  `snapshots/`, `checkpoints/`, `logs/`, `state/`, `local/`.

## Commands

Branch-instance lifecycle (top-level verbs; `[instance]` is inferred when run
inside a workspace):

- `newgit init` / `spawn <name>` / `status [name]` / `remove <name> [--purge]`
- `newgit status <instance> --path` — just the workspace path, for scripts
- `newgit reference` — the definition format, every key and default
- `newgit run [instance] -- <command>` — run with exports and ports loaded
- `newgit action <resource>.<action> [instance]`
- `newgit checkpoint [instance] [-m <msg>]` / `undo [instance] [--to <id>]` /
  `checkpoints [instance]`
- `newgit export [instance] --to <dir> [--include <path>] [--exclude <path>]`
- `newgit cleanup [--dry-run] [--purge-archived]`
- `newgit ports [--check]` — claimed ports, or detect a listening one nothing
  claims

Definition management (noun subcommands):

- `newgit tracker create <name> [--audience <a>] [--storage <s>]
  [--merge-with-source]`
- `newgit tracker track <tracker> <path>...`
- `newgit tracker capture|merge|pull|checkout <tracker> [instance]`
- `newgit tracker capture <tracker> --from-store` — seed a lane from the store
  repo's working tree, so the first `spawn` comes up with the content
- `newgit tracker list`
- `newgit resource add <name> --template <template>` / `list` / `templates`

Resource templates: `process`, `pnpm`, `install`, `command-snapshot`,
`command-snapshot-migrations`, `supabase`, `external`. `newgit resource
templates --show <name>` prints one in full without creating anything.

## Two things worth knowing

**`export` fails closed.** Source ships (its audience is everyone) and so do
trackers whose audience is `public`. Anything narrower is withheld and
reported; `--include <path>` overrides. It writes one commit, not history,
because exporting the branch's commits would carry any file they contain —
including withheld ones. This is a path-level filter, not concealment.

**`[[render]]` puts this instance's ports in the config file your tool
actually reads.** Most tools take a port from a committed config file rather
than argv, so a resource declares literal substitutions into one:

```toml
[[render]]
path = "supabase/config.toml"
replace = [
  { find = "port = 54321", with = "port = {{ports.api}}" },
]
```

There is no template file — `port = 54321` is your working default, so a
clone without newgit still starts on it. `find` is literal, never a regex,
and must match exactly once; that check is also the drift detector, failing
the bind by name when the default changes upstream rather than quietly doing
nothing. Rendered source files are marked `--skip-worktree`, so they never
show in `git status` and `git add -A` cannot commit them — which also means
**real edits to a rendered file do not survive the workspace.** newgit says
so at bind. For a tracker-owned file, `tracker capture` reverses the
substitution, so edits you make beside the rendered value reach the lane and
your port does not.

**`cleanup` never breaks an undo.** It finalizes instances whose workspace is
gone, deletes unclaimed workspaces and dead process state, and prunes tracker
snapshot revs nothing references — but never a rev a checkpoint still points
at, and never a checkpoint record. `project`- and `user`-owned resources are
never torn down by per-branch cleanup.

The one exception is one you ask for. An archived instance's checkpoints are
unreachable — `undo` needs a binding record — yet they go on pinning revs, so
`newgit remove <name> --purge` and `newgit cleanup --purge-archived` discard
that history and release what it held. Live instances are never affected, and
ordinary `cleanup` says how many of its retained revs the flag would free.
