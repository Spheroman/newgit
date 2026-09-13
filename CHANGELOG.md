# Changelog

Notable changes to newgit. Versions follow [semver](https://semver.org),
with the pre-1.0 caveat that the `0.x` series may change the on-disk format
of `.newgit/` in a minor release — see *Upgrading* below.

## [Unreleased]

### Added

- `newgit tracker capture <tracker> --from-store` seeds a lane from the store
  repo's working tree and sets the lane head, so the first `spawn` comes up
  with the content ([#9](https://github.com/Spheroman/newgit/issues/9)).

  A lane starts empty and `capture` reads from an instance workspace, so
  adopting newgit for a tracker carrying `.env` files meant spawning an
  instance guaranteed to come up without them, copying the files in, capturing,
  merging, and re-running the resource action that had already failed. The
  content was in the store repo at the same relative paths the whole time.
  That bootstrap is now two commands: `tracker track`, then
  `tracker capture --from-store`.

  Declared paths with nothing behind them are reported as warnings rather than
  silently seeding a partial lane; a tracker with nothing at all on disk is an
  error, not an empty lane head. `newgit tracker track` now suggests
  `--from-store` when the paths it just added already have content.

## [0.1.1] — 2026-09-12

### Fixed

- `newgit --version` reported `(unknown)` instead of the commit for every
  binary installed from crates.io. The build stamp was derived by running
  `git rev-parse`, but a published `.crate` tarball ships no `.git`, so the
  stamp only ever worked when building from a clone — the one case where you
  can already see the commit. It now reads the `sha1` that `cargo publish`
  records in `.cargo_vcs_info.json`, falling back to `git` for repo builds.
  A tarball packaged with `--allow-dirty` gets the same `-dirty` suffix a
  dirty clone does.

## [0.1.0] — 2026-09-12

First release. All seven milestones of the v1 MVP are implemented, and the
product promise holds end to end: given a source revision plus tracker and
resource definitions, newgit materializes a consistent branch instance and
can put the whole thing back.

### The two primitives

- **Trackers** — named, versioned lanes of file content, each with an
  audience, storage, and source-merge policy. `capture`, `merge`, `pull`,
  and `checkout` move content through content-addressed lanes; identical
  captures dedupe. `source` is just the default tracker, special only
  because Git owns its history.
- **Resources** — lifecycle units for state that cannot travel as content:
  processes, ports, installs, databases, and things another system owns.
  Command-based hooks, deterministic port allocation, exports, dependency
  ordering, and PID-file supervision for long-running actions.

### Commands

`init`, `spawn`, `status`, `run`, `action`, `checkpoint`, `undo`,
`checkpoints`, `export`, `remove`, `cleanup`, plus `tracker` and `resource`
subcommands for definitions. Instance names are inferred from the workspace
you are standing in.

### Checkpoints and undo

One coherent snapshot across source (including uncommitted and untracked
work), every tracker lane, and every resource's state. `newgit undo` restores
it; it takes a safety checkpoint first, so undo is itself undoable and
running it twice is redo. Failed resource restores leave a recovery record
instead of a half-explained failure.

### Export

`newgit export --to <dir>` writes an ordinary Git repository. Tracker
audience is the default filter and fails closed at `public`; `--include`
overrides per path and `--exclude` wins over everything. One commit, never
history — exporting the branch's commits would carry any file they contain,
including what the filter just withheld. Path-level filtering only; not a
concealment mechanism.

### Cleanup

`newgit cleanup [--dry-run]` finalizes instances whose workspace is gone,
deletes unclaimed workspaces and dead process state, and prunes unreferenced
tracker snapshots. It never removes a checkpoint record, and never a snapshot
rev a checkpoint still points at. Resource `[cleanup]` hooks run on both
`remove` and `cleanup`, gated by ownership: `project`- and `user`-owned
resources are never torn down by per-branch teardown.

### Templates

`process`, `pnpm`, `command-snapshot` (a daemon-owned database whose
checkpoint deposits into a tracker), and `external` (a resource newgit holds
only a handle to). Templates bring the companion definitions and tracker
lanes they depend on.

### Known limits

Deliberately out of scope for v1, and not bugs: no FUSE projection
(workspaces are full clones), no hermetic builds, no hunk-level privacy, no
native remote, no command shims, and `storage = "remote"` parses but only
`local` is implemented. v1 does not claim security properties — it keeps
non-public tracker content out of Git by construction, not against a hostile
agent.

An instance whose workspace is deleted cannot be re-spawned under the same
name until `newgit cleanup` finalizes it.

## Upgrading

`.newgit/` holds two kinds of thing, and they upgrade differently:

- **Committed definitions** (`config.toml`, `trackers/`, `resources/`) are
  the control plane. Changes here will be called out in this file.
- **Local state** (`branches/`, `snapshots/`, `checkpoints/`, `logs/`,
  `state/`) is derived or disposable. If a `0.x` release changes its format,
  the upgrade path is `newgit cleanup`, then re-spawn — workspaces are
  disposable by design.

Binding records and checkpoints are the exception worth caring about: they
are the only local state that is not reconstructible. A release that changes
their format will say so here explicitly.

[Unreleased]: https://github.com/Spheroman/newgit/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/Spheroman/newgit/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Spheroman/newgit/releases/tag/v0.1.0
