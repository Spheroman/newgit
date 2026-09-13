# Changelog

Notable changes to newgit. Versions follow [semver](https://semver.org),
with the pre-1.0 caveat that the `0.x` series may change the on-disk format
of `.newgit/` in a minor release — see *Upgrading* below.

## [Unreleased]

### Added

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

- `newgit reference [section]` prints one section instead of all 380 lines,
  and bare `newgit reference` now prints a table of contents.

  The reference grew past the point where paging the whole thing to find
  `[ports]` was reasonable. `newgit reference render`, `newgit reference
  ownership`, `newgit reference tracker` — plural forms and unambiguous
  prefixes resolve too, so `trackers` and `template` land where you meant. An
  ambiguous prefix says which sections it matched rather than dumping the
  list. `newgit reference all` is the old behavior, byte for byte.

  Sections are derived from the document's own headings rather than listed in
  the CLI, so a section added to `definitions.md` becomes addressable without
  touching Rust and the two cannot drift. Asking for a `##` section brings its
  `###` subsections with it, so `resource` is the whole resource format.

### Fixed

- `newgit reference` documents `[[render]]`. The feature shipped in 0.2.0 but
  the reference did not learn about it, so the one copy of the definition
  format guaranteed to be wherever the binary is was the one place it was
  missing — exactly the gap `newgit reference` exists to close. The section
  covers `path`, `replace`, `find`/`with`/`count`, and the four rules; the
  template-variable scope table gains a `[[render]] with` row, and the note on
  unresolved placeholders now names `[[render]]` alongside `[cleanup]` as the
  other place they refuse rather than render verbatim.

- `newgit reference` recommends a content-addressed package manager, in the
  one document that travels with the binary. Per-instance installs are the
  single place newgit multiplies a cost instead of absorbing it, and the
  choice that decides how much — pnpm or npm — is made once, early, by someone
  who has usually not read the README's section on it by then. The reference
  had a parenthetical `(a pnpm store)` in the ownership table and nothing
  else. It now says it plainly, with the per-tool costs and the two keys that
  wire a shared store up (`ownership = "user"`, `[identity] paths`).

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
  the key that pulled it in, and says it can be edited or deleted:

  ```
  Added resource `deps` from `pnpm` at .newgit/resources/deps.toml
    also created resource `pnpm-store` (required by deps.depends_on) — edit it, or delete the file if this project doesn't need it
  ```

  Someone who ran `newgit tracker create db-snapshots` by hand and hit
  `already exists at ...` had no way to know a `resource add --template
  command-snapshot` had created it — the error read like a bug in their own
  script. It now says so in one clause.

## [0.2.0] — 2026-09-12

### Added

- `[[render]]` on a resource substitutes per-instance values into a file the
  project commits ([#10](https://github.com/Spheroman/newgit/issues/10)).

  `[ports]` reached commands as `{{ports.x}}` and as an env var, which assumes
  the tool takes its port on argv. Most do not: Supabase reads
  `supabase/config.toml`, Expo reads `.env`, Compose reads `compose.yaml`.
  Everyone who hit this wrote the same section-aware config rewriter inside
  their `prepare` hook.

  ```toml
  [[render]]
  path = "supabase/config.toml"
  replace = [
    { find = 'project_id = "faretable"', with = 'project_id = "faretable-{{branch.slug}}"' },
    { find = "port = 54321",             with = "port = {{ports.api}}" },
  ]
  ```

  There is no template file. `port = 54321` is the project's working default,
  so a clone without newgit still starts on it; newgit substitutes into the
  committed content and writes the result into one workspace. `find` is a
  literal string, never a regex, and must match **exactly once** — which is
  also the drift detector: when the default changes upstream, the bind fails
  naming the file and the string instead of quietly doing nothing. A
  multi-line `find` disambiguates two sections sharing a value, and `count = N`
  declares a genuine repeat.

  A render reads *committed* content — `HEAD`, or the bound lane rev for a
  tracker-owned path — never the working file, so it is idempotent: `undo`
  and `tracker pull` re-render off the binding record and values never
  compound.

  Instance values stay out of everything downstream. Source-owned targets are
  marked `--skip-worktree`, so they never show in `git status` and `git add -A`
  cannot commit them; `newgit export` and a checkpoint's uncommitted-state
  capture both take them from `HEAD`; and `newgit tracker capture` **reverses**
  the substitution for tracker-owned targets, so a key you add to `.env.local`
  reaches the shared lane and this instance's port does not.

  On a source-owned path, skip-worktree also means real edits to that file in
  this workspace do not survive it. newgit reports that **precisely rather
  than as a caveat**: a render is a pure function of committed content and the
  binding record, so the expected bytes are recomputable, and newgit compares
  against them at checkpoint and before every re-render — naming the file and
  how many lines a re-render will discard, and saying nothing when the file is
  what the render produced.

  Replacements are **simultaneous**: every `find` is located in the committed
  content and the whole batch applies in one pass, so a replacement's output
  is never a match target and reordering the `replace` array cannot change the
  result. Two rules claiming overlapping text are refused by name.

- `newgit reference` prints the definition format — every tracker and resource
  key with its type and default, the checkpoint/restore/ownership tables, and
  which template variables are in scope for which hook
  ([#13](https://github.com/Spheroman/newgit/issues/13)).

  All of it was documented in `newgit-v1-mvp.md`, and none of it travelled:
  `cargo install newgit` leaves a binary and a README on disk, and the
  README's relative link to that file resolved to nothing on crates.io,
  docs.rs, or in the registry directory. Reading the crate source — or running
  `strings` on the binary to enumerate `{{...}}` variables — was the only way
  to answer what `checkpoint.mode` accepts. The reference now ships inside the
  binary, which is the one copy guaranteed to be wherever the definitions are,
  and `init`, `tracker create`, and `resource add` each name it. The README's
  links to the design documents are absolute, and say they live on GitHub.

- `newgit status <instance> --path` prints that instance's workspace path and
  nothing else ([#12](https://github.com/Spheroman/newgit/issues/12)).

  A bootstrap script, a README snippet, or an editor integration wanting the
  path had to parse table output or read `.newgit/branches/<name>.toml`, which
  makes the store layout someone else's API. Now:
  `W=$(newgit status auth-refactor --path)`. The instance is inferred inside a
  workspace, and warnings stay on stderr so stdout is a path.

- `newgit remove <name> --purge` and `newgit cleanup --purge-archived` release
  the snapshot revs an archived instance's checkpoints were pinning
  ([#12](https://github.com/Spheroman/newgit/issues/12)).

  Cleanup never prunes a rev a checkpoint points at, which is right for a live
  instance and a dead end for a removed one: `undo` needs a binding record, so
  those checkpoints are unreachable while their revs are permanent. Purging
  drops the checkpoint log and its `refs/newgit/checkpoints/<slug>/*` store
  refs, and the same pass reclaims what they held; a purging `--dry-run`
  reports exactly that. It stays opt-in and never touches a live instance's
  checkpoints. Ordinary `cleanup` now reports how many of its retained revs
  are held only by archived instances, and `remove` says how many checkpoints
  it kept.

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

- `.newgit/scripts/` and a `{{scripts}}` template variable, so a resource's
  script lives under the same rule as the definition that calls it
  ([#8](https://github.com/Spheroman/newgit/issues/8)).

  A resource definition is read from the store, but anything its commands
  shelled out to was read from the workspace, where it is subject to source
  materialization. The two halves of one definition lived under different
  rules and only the TOML half was editable in place, so iterating on a
  `prepare` meant committing every attempt or copying the script into the
  workspace by hand between runs.

  `{{scripts}}` resolves to `.newgit/scripts/` in the store. Edit a script
  and the next `newgit action` runs it — nothing to commit, nothing to copy,
  and it works on the first spawn. The directory is control plane and is
  listed in `init`'s "commit these" output; `init` writes a `README.md` there
  because Git will not track an empty directory. Project-owned scripts are
  unaffected: they stay in the project tree, are read from the workspace, and
  must still be committed before a spawn that calls them.

### Changed

- The `command-snapshot` template's `[actions.migrate]` says what it is: a
  convenience command you invoke with `newgit action <resource>.migrate`, not
  a lifecycle hook. No stage ever ran it, and sitting beside `prepare` — which
  `spawn` and a `recompute` restore do run — it read like one
  ([#12](https://github.com/Spheroman/newgit/issues/12)).

### Fixed

- A declared `captures` name that never appeared in an action's stdout failed
  in total silence: the command exited 0, newgit reported `prepare: ok`,
  marked the resource `ready`, and published an empty handle that went
  unnoticed until an API call returned 401
  ([#7](https://github.com/Spheroman/newgit/issues/7)).

  Each missing capture is now warned for by name, with the log to look in and
  the convention that explains nearly every occurrence — when `captures` is
  set, stdout belongs to newgit, so everything else the command prints should
  go to stderr. Warnings appear at `spawn`, on `newgit action`, and during a
  recompute restore. A missing name is still not an error: a resource may
  legitimately publish a handle only on some runs. The `external` template's
  comments now state the stdout convention too.

- An undo where a resource restore failed reported `Restored` on its first
  line and `FAILED` on its fourth, describing one operation two ways
  ([#11](https://github.com/Spheroman/newgit/issues/11)). A restore command
  is not transactional — one that rebuilds a schema and then fails to load
  the rows leaves its resource in neither the pre-undo state nor the
  checkpoint state — so the summary no longer implies the instance is in a
  known state:

  ```
  Undo of `smoke` to ckpt_001 ("before agent") INCOMPLETE: 0 of 1 resources restored
    `db` may be in a partial state — a failed restore command is not rolled back
  ```

  `newgit undo` now exits non-zero when the undo was incomplete.

- A failed undo left a pre-undo checkpoint indistinguishable from one a human
  named, so three failed attempts left three of them, each pinning its
  tracker revs. The safety checkpoint now records `undo_completed` once the
  undo it preceded finishes, and `newgit checkpoints` shows those entries as
  `failed-undo` rather than `before-undo` — they are not redo points. The
  record is annotated rather than deleted: newgit cannot know at save time
  whether the undo will succeed, and discarding the only record of a state is
  what checkpoints exist to prevent. Releasing the revs those entries pin is
  a `cleanup` concern, tracked in
  [#12](https://github.com/Spheroman/newgit/issues/12).


- A resource whose `depends_on` named something that did not exist yet made
  *every* newgit command fail, including `tracker create` and `tracker track`
  — the commands that create the missing name. The only way out was to hand-
  edit the `depends_on` line, run the command, and put the line back
  ([#6](https://github.com/Spheroman/newgit/issues/6)).

  The dependency graph is now resolved leniently at load and its problems
  reported rather than raised. Commands that *act* on the graph — `spawn`,
  `run`, `action`, `checkpoint`, `undo` — still refuse, with the same error
  naming the missing dependency. Commands that *build* it (`tracker create`,
  `tracker track`, `resource add`) and commands that inspect it (`status`,
  `tracker list`, `resource list`) now run and print the problem as a warning.
  `remove` stays reachable too, so teardown never depends on the graph holding
  together. Dependency cycles are handled the same way.

### Documentation

- Say what a package manager costs under newgit. Every instance installs its
  own dependencies — that independence is the point, and a shared installed
  tree across branches with different lockfiles is the thing that would be
  wrong — but the size of that cost is the package manager's call, and
  nothing said so. pnpm hardlinks from one content-addressed store, so
  instance ten adds directory entries; `npm ci` expands a full copy per
  instance, which on a monorepo is gigabytes each. Documented in the README's
  adoption section, next to the install resource in `newgit-v1-mvp.md`, and
  in the `pnpm` template's own comments, since that is what someone reads
  when they hand-edit the definition.

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

### On-disk format

Binding records (`.newgit/branches/<name>.toml`) gained a `rendered` list on
each resource binding, recording the substitutions a render applied. It is
optional on read, so records written by `0.1.x` load unchanged and behave
exactly as before — an instance with no `[[render]]` in its resources has no
`rendered` list to write. Nothing else in `.newgit/` changed shape, and no
`cleanup` or re-spawn is required to upgrade.

Rolling *back* to `0.1.x` with records `0.2.0` wrote does not error — the
field is simply unknown to it — but it is lossy: the old binary drops
`rendered` the next time it saves that record, and with it the substitutions
`tracker capture` needs in order to reverse a render. Re-spawning the
instance under `0.2.0` restores it.

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

[Unreleased]: https://github.com/Spheroman/newgit/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Spheroman/newgit/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/Spheroman/newgit/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Spheroman/newgit/releases/tag/v0.1.0
