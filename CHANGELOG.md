# Changelog

Notable changes to newgit. Versions follow [semver](https://semver.org),
with the pre-1.0 caveat that the `0.x` series may change the on-disk format
of `.newgit/` in a minor release — see *Upgrading* below.

## [Unreleased]

Unreleased entries live in [`changelog.d/`](changelog.d/), one file per
change. They are folded into this file under a version heading at release
time by `scripts/release-changelog.sh`, which then deletes the fragments.
Two changes never touch the same lines, so nothing queues behind whichever
branch landed first.

## [0.7.0] — 2026-10-28

An instance can be put down and picked up again. `newgit stop` and
`newgit start` take every resource of an instance down and back at once,
`supabase` containers included, so T3 Code frees a settled thread's ports
and memory without losing it, and `autostart` has a spawn bring up its dev
server. Claude Code's worktrees get instances of their own, newgit installs
with one line on any Linux distro, and a long list of fixes makes what
`spawn`, `undo` and `status` report match what happened. See *Upgrading*.

### Added

- `newgit stop [instance]` and `newgit start [instance]` stop and start an
  instance's resources at once
  ([#157](https://github.com/Spheroman/newgit/issues/157)).

  The only lever was `newgit action <resource>.<action>`, one resource at a
  time, so a caller releasing an instance had to know every resource's
  actions and order them itself. `stop` stops every running long-running
  action, dependents first, and records what it stopped; `start` brings
  back exactly that, dependencies first, and nothing nobody had started.
  Both are idempotent and exit 0 with nothing to do, so T3 Code can call
  them on every settle and unsettle; `--json` says which resource failed,
  and exit 1 says that one did. Unlike `remove`, the workspace, trackers,
  installs, and port claims stay, so the instance comes back on the same
  ports. `remove` now stops running resources dependents first too.

- `newgit stop` and `newgit start` take down and bring back a resource that
  is not a process newgit supervises, through a `suspend` / `resume` action
  pair it defines ([#157](https://github.com/Spheroman/newgit/issues/157)).

  `stop` only signalled supervised processes, so a settled T3 Code thread
  whose `prepare` ran `supabase start` kept a dozen containers, nine ports
  and a gigabyte or two of memory until the thread was deleted. `stop` now
  runs `suspend` after a resource's dependents are down, and `start` runs
  `resume` before they come back; the `supabase` template defines the pair
  as `supabase stop` / `supabase start`, which keeps the data. `suspend`
  never runs for a `project` or `user` resource, which other instances
  share, nor for one that never came up. Each needs a command, and neither
  is accepted without the other.

  `start` no longer starts a resource whose dependency did not come back in
  the same `start`: it fails naming the dependency and stays owed, so a dev
  server is not brought up against a database that is still down.

- `autostart = true` on a long-running action has `newgit spawn` start it
  once every `prepare` has run, dependencies first, so a spawned instance
  comes up with its dev server running. The `process` template sets it.

  Before, spawn ran only `prepare`, and a dev server waited for someone to
  run `newgit action app.start`; T3 Code and Claude Code never did. A start
  that fails is reported like a failed `prepare`: the spawn says which
  resource did not come up and exits 1, and a resource that depends on it
  is not started against it. `spawn --json` lists each attempt under
  `started`, in the shape `newgit start --json` uses. `spawn
  --from-checkpoint` starts them after its restore, and `newgit tracker use`
  starts those of the resources a tracker brings, which `drop` stops. An
  existing resource definition is unchanged until you add the key.

- `newgit claude-code install` gives Claude Code's worktrees newgit
  instances: `claude --worktree <name>`, a subagent with `isolation:
  "worktree"`, and a background session each spawn an instance named for
  the worktree, with its trackers projected and its resources prepared,
  and removing the worktree runs `newgit remove`.

  It writes Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks into
  `.claude/settings.local.json` (`--shared` writes `.claude/settings.json`
  instead), and refuses beside another `WorktreeCreate` hook rather than
  have two race to make the worktree. The hooks are `newgit claude-code
  worktree-create` and `worktree-remove`, which read Claude Code's hook
  input on stdin and print nothing on stdout but the workspace path. A
  worktree name already in use resumes its instance, and each workspace
  gets the hooks in its own Git-excluded `.claude/settings.local.json`,
  since Claude Code reads the remove hook from the directory the session
  ends in.

- `newgit export --remote <name>` brings in what was merged on the target
  before it pushes. When the target's branch is ahead of the export (a pull
  request merged on GitHub, a dependabot bump), the project root
  fast-forwards to it and it merges into `main`, as `git pull` then
  `newgit merge main` did by hand. Before, the push was refused as not a
  fast-forward. When the target's commits are not the project's own (the
  project root has commits of its own, or the target withholds a tracker),
  it refuses and says how to bring them in. `--dry-run` reports how many
  commits would come in; `--force` still replaces the branch instead.

- One-line install: `curl -fsSL https://newgit.dev/install.sh | sh` picks the
  release binary for your platform, verifies its checksum, and installs it to
  `~/.local/bin`. Or `brew install spheroman/newgit/newgit`; each release
  updates the tap's formula.

- `newgit remote delete <owner>/<name>` deletes a project from its newgit
  remote: every token pinned to it is revoked, every repo under its prefix
  deleted, and its record cleared, so the name can be linked again from
  scratch. Owner only, from a login rather than an agent's or workspace's
  token, and the project is typed out to confirm. It removes `[remote]` from
  `.newgit/config.toml`.

- A `[[render]]` can target a file inside its resource's own `[identity]
  produces`, so the install store can share a tree that names its own
  workspace.

  Until now the store declined any tree that mentioned the absolute path of
  the workspace it was built in, and pnpm writes that path into
  `node_modules/.pnpm-workspace-state-v1.json`, so no pnpm workspace was
  ever stored. A render into a built tree treats the file as rendered for
  the instance that built it: storing reverses it to `find`, and filling
  renders it forwards with the filled instance's `{{workspace}}`. The stored
  copy is still scanned, so a path the render does not cover still keeps
  the tree out. `count` is refused on such a render, and `render --check`
  skips it. The `pnpm` template now declares one for the workspace state
  file. On T3 Code, a second `newgit spawn` now takes about 5s instead of
  50s.


### Changed

- The Linux release binaries are static musl builds
  (`newgit-<arch>-unknown-linux-musl.tar.gz`), replacing the glibc builds.
  Those needed glibc 2.39, so they did not run on Debian 12, Ubuntu 22.04,
  RHEL 9, Amazon Linux 2023, or Alpine. The static builds run on all of them.


### Fixed

- The `init_in_a_workspace_is_refused` test no longer flakes on Linux
  ([#204](https://github.com/Spheroman/newgit/issues/204)).

  It walked the configured workspace root two levels deep, as if a
  per-project directory sat between it and the workspace, and so listed the
  workspace's own contents and ran `newgit init` from whichever entry
  `read_dir` returned first. ext4 orders entries by filename hash, so a file
  came first on some runs and `current_dir` failed with `ENOTDIR`. The test
  now uses the workspace path spawn writes.

- The `supabase` template allocates and renders every port `supabase init`
  writes, so a second instance's render is no longer refused
  ([#205](https://github.com/Spheroman/newgit/issues/205)).

  The template allocated six ports, but `supabase init` writes nine in the
  same 543xx block: the disabled pooler's `port = 54329` and the
  commented-out `# smtp_port = 54325` and `# pop3_port = 54326` as well.
  Once a first instance held the six, the second instance's scan landed on
  those numbers, and the render refused: `port = 54325` also matches inside
  `# smtp_port = 54325`, so the file would name the value twice and
  `newgit capture` could not reverse it. The template now has `smtp`,
  `pop3` and `pooler` lanes, rendered like the others, so those ports also
  move per instance once enabled or uncommented. A project that added the
  template before this has its own copy under `.newgit/resources/`, and
  `resource add` will not overwrite it. Copy the three `[ports]` lines and
  their three `[[render]]` rules from `newgit resource templates --show
  supabase` into that file.

- Changing a live instance's binding rebuilds the installs keyed on it
  ([#206](https://github.com/Spheroman/newgit/issues/206)).

  Since #153 the install store keys a tree on the trackers its instance
  binds, because a shared install builds the bound members, named by
  `{{bound.paths}}` or found on disk. Two paths changed the binding of a
  live instance and left such a tree as it was. `tracker use` and `drop`
  only started and stopped the resources the tracker brings, and a shared
  install is brought by `root`, which is always bound, so an instance that
  bound `b` kept the tree built without it until someone ran `prepare` by
  hand. And undo's `recompute` skip compared the checkpoint's identity hash
  with the pre-undo one, which the binding is not part of, so an undo that
  restored a different binding kept the tree built for the one it replaced
  and reported `skipped: identity unchanged`.

  `tracker use` and `drop` now rebuild every bound install that holds a
  tree once the binding has moved, the same way a spawn builds it: filled
  from the store when another instance built that binding already, else by
  running its `prepare` and publishing the result. The command reports each
  rebuild; one that fails leaves the resource `failed` without failing the
  command, with `newgit action <resource>.prepare` as the retry, and what
  depends on it is reported as not rebuilt and left `pending`. A `use` that
  brings what a tracker `requires` binds their resources once all of it is
  bound, so they are built for the whole binding. Undo's skip now requires
  the binding it restored to be the one the instance had, for a resource
  that declares `[identity] produces`, so it compares the inputs the store
  key does; an undo that drops or brings back a tracker rebuilds such a
  tree, through the store first.

- `[identity] key_command` is rendered before it runs, like an action
  `command`, and the reference's *Template variables* tables list
  `{{bound.trackers}}` and `{{bound.paths}}`
  ([#207](https://github.com/Spheroman/newgit/issues/207)).

  The reference said every command is rendered, but `key_command` was
  handed to the shell verbatim, so `{{bound.paths}}` in one printed as
  `{{bound.paths}}`. It now gets the scope an action command gets: branch,
  workspace, scripts, this resource's ports and `{{bound.*}}`. The binding
  is already part of every key; `{{bound.paths}}` lets the command pass it
  to what it inspects. The reference now warns against printing
  per-instance values like `{{workspace}}`, which would key every instance
  apart. The two `bound` variables were described everywhere but the
  tables that are supposed to list every variable and where it has a
  value; they now have rows and a scope column, and the scope table has a
  `key_command` row.

- `spawn --from-checkpoint` now exits 1 when the instance it leaves is not
  ready, and says why: each restored resource's line carries the restore's
  verdict (`ok` or `FAILED`), as `undo` prints it, and a failed restore's
  recovery record is named on stderr
  ([#208](https://github.com/Spheroman/newgit/issues/208)).

  #86 and #124 gave the plain `spawn` and both `--json` forms an exit code
  that says what the summary says, and left the human-readable
  `--from-checkpoint` behind: it printed `resource: seed command` and exited
  0 whether the command had succeeded or not, so the only place the failure
  showed was a later `newgit status`. It now judges by the same condition
  `--json` does: a failed restore, or a resource the checkpoint held no
  state for that failed to bind, both exit 1. The headline names what did
  not come up, and every resource the instance has gets a line — one the
  restore did not touch shows its bind's `prepare` verdict, as a plain
  `spawn` prints it, and so does one whose restore ran nothing (no
  `[restore]`, `none`, `external`) and left it as a failed bind had it. The
  instance and its record are kept, as before.

- A dependent with no `prepare` is reported blocked by every surface when
  its dependency fails, `spawn --json` included
  ([#209](https://github.com/Spheroman/newgit/issues/209)).

  Such a resource is recorded `ready` on purpose (#28: nothing was withheld
  from it, and nothing would ever rerun to move it off `pending`), and
  `status`, `action` and the human-readable `spawn` derive blocked-ness
  from the graph at read time instead. `spawn --json` read the record's
  `ready` straight through with an empty `blocked_by`, so a script saw the
  resource ready while `status` said `blocked(base)` and `action` refused
  to start it. It now derives `blocked` and `blocked_by` the same way the
  other surfaces do. A script that treated `ready` with a failed
  dependency as fine will now see `blocked`.

  The human-readable `spawn` also said `prepare: BLOCKED by base` for a
  resource with no `prepare`, then counted only the failed dependency in
  "1 of 2 resources did not bind". It now says `BLOCKED by base (no prepare
  to run; its actions are refused until that is repaired)`, and the count
  includes every blocked resource, as the spec already said it should.

  A resource that failed itself (its export, render or `prepare`) with a
  dependency down too is now `failed` in `status`, as it already was in
  `spawn` and `spawn --json`, rather than `blocked(...)`: repairing the
  dependency would not fix it.

- `undo` to a checkpoint taken by `tracker drop --discard` no longer brings
  back a file that was deleted when the checkpoint was taken
  ([#210](https://github.com/Spheroman/newgit/issues/210)).

  Undo restored the source while the tracker was still dropped, when its
  paths were skip-worktree entries the restore leaves alone, so a deleted
  file stayed in the index and came back from HEAD when the tracker was
  bound again afterwards. The binding is now restored before the source,
  so the restore sees the tracker's paths as ordinary tracked files and the
  deletion holds, as it already did for paths outside a dropped tracker.

- `undo` no longer prints a bare `ok` for a resource it leaves failed,
  blocked or pending ([#211](https://github.com/Spheroman/newgit/issues/211)).

  Since #137 a restore that runs nothing (`none`, `external`, no
  `[restore]`) leaves a resource's status as it found it, so a `prepare`
  that failed before the undo is still failed after it. The report's `ok`
  is the verdict on the restore action, which could not fail; under
  "Restored" it read as the resource being fine while `status` showed it
  failed. The line now adds what the resource is left as, the way `status`
  derives it — `none ok, left failed`, `none ok, left blocked by deps` — and
  `UndoResourceOutcome` carries it as `status` and `blocked_by`. A restore
  that was ok but whose restart failed now says `restart FAILED` and counts
  against "N of M resources restored" and the "may be in a partial state"
  list; before, that list could name nobody.

- Two commands on one instance no longer run over each other
  ([#212](https://github.com/Spheroman/newgit/issues/212)).

  Nothing serialized them: two overlapping checkpoints minted the same id
  and the one saved last replaced the other, and an `action` saved back the
  whole record it had loaded before a parallel one changed it, so a failed
  `prepare` came back as ready. Every command that changes an instance now
  holds that instance's lock (`.newgit/local/locks/<slug>.lock`, an
  advisory file lock the kernel releases with the process, so a crash
  leaves nothing stale) from start to finish, and a second command on the
  same instance waits, saying so. A hook run under the lock sees
  `NEWGIT_LOCKED`, the locks it runs under; a `newgit` it starts that would
  need one of them refuses instead of waiting for its own ancestor. Records
  are also written to a temporary file, flushed, and renamed into place, so
  a reader — `status`, `spawn`, anything that lists instances — never
  parses one half-written, and a crash mid-write leaves the record it
  found.

- `spawn --json` now exits non-zero when a resource did not come up, and
  says which: the follow-up to
  [#86](https://github.com/Spheroman/newgit/issues/86), which fixed only
  the human-readable `spawn`.

  #86's check ran only after the `--json` branch had printed and returned,
  and that branch printed nothing about resources, so a failed `prepare`
  exited 0 with nothing in the output saying so — to a script, the caller
  most likely to ask for JSON and the one that goes by the exit code. Each
  resource is now listed, by name, with its `status` (`ready`, `failed`,
  `blocked` or `pending`), its ports, its `prepare` log, any render or
  export error, and `blocked_by`: the dependencies that are not ready, for
  a resource that is not ready itself. The exit is the same `exit 1`,
  after the JSON; the instance and its record are kept, as they are
  without `--json`.

  `spawn --from-checkpoint --json` had the same gap, and now judges by the
  instance the restore leaves: a failed restore exits 1, and so does a
  resource the checkpoint never had that failed to bind. Status and
  `blocked_by` describe that instance, not the bind the restore ran after,
  and `recovery_record` names the record of a failed restore. Its
  `resources` array changed shape, which matters to a script that reads
  it: it now lists every resource the instance has, by name rather than
  in restore order, and `action` and `ok`, before always a string and a
  bool, are null for a resource the restore did not touch. A script that
  requires every `ok` to be true now fails on such a resource unless it
  skips the nulls.

- Code trackers are carved in the order they nest: `project` (`.newgit`)
  before `access` (`.newgit/trackers`), which carves out of it. By name,
  `access` came first and found nothing to carve, so a project whose
  history already held `.newgit/` (a clone adopted with `newgit init`)
  failed `remote add` with "refusing to merge unrelated histories".

- A checkpoint no longer captures an instance's rendered values into a
  secret tracker's lane, and neither does the safety capture `tracker
  pull` takes before it re-binds. Like `tracker capture` and `tracker
  checkout`, both now reverse the `[[render]]` substitutions in files the
  tracker owns for the capture, so the lane gets the committed values and
  any edit made beside them.

  The lane is shared by every instance, so an instance's allocated port
  did not stay with it: once `tracker merge` made such a rev the lane's
  `main`, each instance spawned after that got that port in the file and
  failed to render it. The checkpoint also records the rev as the file's
  committed content, which the render's `find` no longer matched, so
  `undo` to it warned that the re-render failed.

  A rendered file holding exactly its committed content, which is what a
  tracker restore leaves, is no longer reported as having "changes that
  render will discard": a re-render loses nothing in it. With the lane
  fixed, `undo` would have printed that after restoring such a file;
  `tracker checkout` and `tracker pull` already returned it, in warnings
  the CLI does not print.

- Two `newgit spawn`s that overlap are no longer promised the same port.
  Agents spawning side by side is the case newgit is built for, and the
  instance that came up second could not bind its port, or reached the
  other instance's service on it.

  The binding records are the only claim on a port, and `spawn` wrote its
  record only after every `prepare` had run, so a spawn that started
  while another was still installing found no claim on the port that one
  had been given, and nothing listening on it yet either. Each resource's
  ports are now written into the record before its `prepare` runs, under
  a lock (`.newgit/local/ports.lock`) held only while they are chosen, so
  spawns still install side by side. `tracker use`, and an `undo` that
  binds resources, claim their ports the same way.

  The record is now written as soon as an instance's resources start to
  bind, not once they are prepared: `newgit status` shows an instance
  that is still spawning, and one whose spawn failed or was killed after
  that point keeps its record, and with it its ports, until `newgit
  remove`. Each resource is in that record, `Pending`, with its exports
  and rendered files, before its `prepare` runs, so after a spawn killed
  in `prepare`, `newgit action <resource>.prepare` runs it with the
  resource's exports, and newgit still knows which files it rendered.

  A resource is `Pending` in that record while its `prepare` runs, so
  readiness no longer assumes a `Pending` resource is waiting on a
  dependency, where it said it "never came up (a dependency of it did
  not)": with no dependency blocking it, it says the resource "has not
  been prepared" and to run `newgit action <resource>.prepare` unless
  newgit is still preparing it.

- The Git endpoint at `https://newgit.dev/<owner>/<name>.git` answers again.
  Since the endpoint learned to sleep, its object set the container's
  inactivity backstop in its constructor, before any container had started;
  that throws, so every clone and fetch failed with a 500. The backstop is
  now set once the container is running.

- `newgit init` where `.newgit/` already exists adopts it instead of
  refusing. A plain `git clone` of a newgit project, or a project set up
  before trackers had histories, left `root` with no history, so `remote
  add`, `merge main`, and `export --remote` failed with "its history shares
  no commit with the trackers' histories". `init` now writes any missing
  built-in tracker definitions and gives `root` the project's history, as a
  fresh `init` does; that error names it.

- The install store keys a tree on the trackers its instance binds, so an
  instance is no longer filled with a tree built for a different binding:
  one that drops a tracker got the tree built for an instance that binds
  it, and the other way round.

  A shared lockfile lists every member, bound or not, and its install
  builds the bound ones, named by `{{bound.paths}}` or found on disk, where
  a dropped tracker's files are not checked out. The key held the identity
  paths' content, `key_command`'s output and the definition, and a shared
  lockfile does not change with the binding, so the tree built for one
  binding filled instances of another. docs/design/trackers.md already said the
  bound set was part of the install's identity.

  The binding is in every resource's key, not only those whose commands
  mention `{{bound…}}`, because an install can read it without naming it:
  an unfiltered `npm ci` links a workspace member only when the member is
  checked out, and a check for the placeholder would have left that case
  as it was. The cost is cache hits. Instances whose bindings differ no
  longer share a tree for any resource, even one the binding cannot
  change, such as a package's own install in its own tracker, so each
  binding installs once. Instances that bind the same trackers still
  share.

  Every existing store entry is re-keyed, which costs one reinstall per
  key: the first spawn at each new key installs and stores its tree, and
  `newgit cleanup` reclaims the old entries, which nothing keys to any
  more.

- On macOS the install store clones a stored tree with one `clonefile(2)`
  call per produced path instead of `cp -c`.

  `cp -c` clones one file at a time, so on a large pnpm monorepo a cache
  hit was slower than the install it replaced: filling T3 Code's 264k-file
  `node_modules` took 37s, against 17s for `pnpm install`. A cache-hit
  `newgit spawn` of that repo now takes about 5s instead of 40s. `cp` is
  still the fallback, and Linux keeps `cp --reflink`.

- `newgit remove`, and `newgit cleanup` finishing an instance whose
  workspace is gone, no longer refuse while the resource graph does not
  resolve. Teardown is the way out of a broken project, and 0.2.0 promised
  that `remove` would never depend on the graph holding together.

  The promise held only for instances with no `[cleanup] command` to run.
  A hook's environment came from the same assembly as `newgit run`'s,
  which refuses any graph that does not resolve, so one bad `depends_on`
  anywhere made every instance with a hook impossible to remove, even one
  that binds only valid resources. A hook now gets that environment layered
  in whatever order the graph has. An edge that does not resolve is skipped
  rather than fatal, so every variable is there.

  The order decides only a name two declarations claim, and that is where
  teardown still refuses: when both reach the instance with different
  values, a hook would get whichever is layered last and could tear down
  the wrong thing with it. An existing instance meets that when a port's
  `env` is set or renamed onto a name another of its resources already
  uses, since a port's `env` is read from its definition each time. The
  refusal names the collision and leaves the instance untouched until it
  is renamed. A collision that does not reach the instance, such as one
  with a resource added after the spawn, does not block it. `newgit env`
  still refuses on any graph that does not resolve, so it cannot show a
  teardown's environment beforehand.

- Tracker paths are checked by where they lead rather than how they are
  spelled, and a code tracker may own other trackers' definitions: the
  governance layering docs/design/trackers.md describes under "Who may change
  what".

  The check read a path's first component as written, so `./.git`, which
  leads with `.`, got past the guard that refuses `.git`: a secret tracker
  could claim the workspace's Git directory, and its captures would sweep
  up the files in it. `tracker track` also kept `./.env.local` as written
  and ignored it as `/./.env.local`, which Git matches to no file, so the
  secret was not ignored at all. Paths are now normalized before they are
  checked and stored, and `.git` and `.newgit` are refused in any case,
  since a case-insensitive file system, macOS's default, opens `.GIT` as
  `.git`. A definition that already claims such a path no longer loads,
  and the error names the path.

  The same guard refused every claim inside `.newgit/`, including the one
  the design rests on: who may change a tracker's paths and audience is
  whoever may change the tracker owning its definition file, so the
  maintainers of an admin tracker owning `.newgit/trackers/web.toml`
  decide who reads `web`. The remote resolves such a claim, but locally
  the project would not open. Those files are all that opens up, and only
  to code trackers, since a secret tracker would keep the definition out
  of Git. The rest of `.newgit/` stays with `project` and `access`.

- `undo` no longer runs a resource's `[restore] command` with nothing to
  restore from, after a checkpoint that went ahead without the resource's
  dump or handle because it was not up. With any bundled database
  template, that restore emptied the database once it was back up. This
  was disclosed under "Not in this PR" in
  [#137](https://github.com/Spheroman/newgit/pull/137).

  A checkpoint goes ahead without a resource that is not up and whose
  capture fails, and `newgit checkpoint` says an undo to it "leaves them as
  they are". The undo ran that resource's `[restore] command` all the same,
  with `{{state_ref}}` unrendered, and every bundled database template
  drops or resets the database before it reads the dump. `spawn
  --from-checkpoint`, which ends in the same undo, ran it in the new
  instance, and a redo did the same to a resource that was failed when the
  undo it redoes began, since that undo's safety checkpoint skips it. When
  the checkpoint is `command` or `external` and the restore is a `command`,
  the undo now leaves that restore out: the resource keeps its state and
  status, and a warning names it. A process it was running is still
  stopped with the rest and started again after its dependencies, and the
  undo reports the resource as `not restored + restarted`; a restart that
  fails is reported like any other. That holds for checkpoints taken
  before this fix too. A `recompute` restore still rebuilds such a
  resource, since it ignores the ref. `newgit undo` prints no `resource:`
  line for a resource it leaves out and did not stop, `spawn
  --from-checkpoint --json` reports such a resource's `action` and `ok` as
  null, and `checkpoint --verify` reports a resource it could not capture
  as `NOT CAPTURED` when its restore runs something, and does not call the
  instance verified.

  A `command` restore whose `{{state_ref}}` has no value in the checkpoint,
  such as one behind a `command` checkpoint that printed nothing, is now
  refused rather than handed the literal placeholder, and the undo reports
  it as a failed restore.

- `undo` no longer marks a resource `ready` just because its restore had
  nothing to run, so one whose `prepare` failed, or never ran behind a
  failed dependency, stays `failed` or `pending` after an undo.

  A `none` or `external` restore, a resource with no `[restore]`, and a
  `recompute` skipped on unchanged identity all succeed without running
  anything, and undo took that as proof the resource was up: `newgit
  status` showed it and its instance `ready`, its dependents stopped
  counting as blocked, and `spawn --from-checkpoint`, which ends in the
  same undo, recorded a `prepare` that had just failed as `ready`. Such a
  restore now leaves the status as it was; one that runs a command still
  sets `ready` or `failed` by its result. The warning about a failed
  resource the safety checkpoint skips no longer blames a restore when it
  was the `prepare` that failed. A resource an earlier undo already marked
  `ready` stays so until `newgit action <resource>.prepare` runs again.


### Documentation

- The design documents moved from the repository root into `docs/design/`:
  `spec.md` (was `newgit-v1-mvp.md`), `trackers.md`, `remote.md` (was
  `newgit-cloudflare.md`), `vision.md` (was `newgit-architecture.md`), and
  `hosts.md`. `spec.md` is rewritten as the design of newgit as it is
  rather than as a v1 plan, and leaves every key and template variable to
  `newgit reference`. `vision.md` is labelled as the original vision, most
  of it unbuilt. The tracker and remote docs, the README, and the guide are
  brought in line with the code and list the open issues they know of.
  `newgit reference` documents `[remote]` in `config.toml` and your own
  `~/.newgit/config.toml`, shows sample tracker files, and says that
  `merge_with_source` is stored but not yet read.

- The README states the git newgit needs: 2.38 or newer, for
  `git merge-tree --write-tree`. The install script warns when git is older.

- Rewrote the README to explain what newgit is for and how to get started.
  The full workflow, every command, and the details worth knowing moved to
  `docs/guide.md`.

## [0.6.0] — 2026-10-26

GitHub stays where it is. `newgit export --remote` pushes the trackers you
choose to an ordinary repository such as a GitHub mirror, history and all,
and with `branches = true` every push from a workspace lands its branch
there too, so an agent's `git push` then `gh pr create` opens a real pull
request even on a newgit remote. No formats change; see *Upgrading*.

### Added

- `newgit export --remote <name>` pushes the trackers' `main`s, composed
  with their history, to an ordinary Git repository such as a GitHub mirror,
  defined in `.newgit/exports/<name>.toml`.

  Nobody moves their code off GitHub to try a tool, so newgit has to sit
  beside it: a project can live in newgit while its GitHub repository, and
  everything that reads it (`gh`, Actions, clones), keeps working. What
  ships fails closed: `public` code trackers, plus the ones a target names
  in `trackers`, so a public mirror and a private one can show different
  views. A secret tracker cannot be named, and a history that ever added a
  secret tracker's path is refused before anything is pushed. Pushing uses
  plain `git push` with your own credentials; exporting again
  fast-forwards, and a rewritten history (a narrower tracker set) needs
  `--force`.

- Export targets with `branches = true` receive each instance's branch
  every time its workspace pushes, and on a newgit remote the target
  becomes the workspace remote `gh` resolves.

  On a newgit remote, a workspace's `origin` is the newgit endpoint, so an
  agent's reflexive `git push` then `gh pr create` failed: `gh` cannot map
  that URL to a GitHub repository, and the branch never reached GitHub. Now
  the push exports the branch, the workspace's `refs/remotes/<target>/`
  holds the commit the target really has, and `gh pr create` works as is
  when the target ships every tracker the branch touches. When it does
  not, the commits there differ, and the push says `gh pr create --head
  <branch>` instead of pretending otherwise. A push straight to the
  target's remote is refused, since it would carry trackers the target
  withholds.

### Fixed

- `undo` restarts a long-running action in its `workdir`, as `newgit
  action` does, rather than in the workspace root
  ([#109](https://github.com/Spheroman/newgit/issues/109)).

  The restart built its own invocation and handed the supervisor the
  workspace root, so a dev server with `workdir = "apps/web"` came back in
  the wrong directory after an undo, where `npm run dev` fails or runs the
  wrong package. newgit reported it restarted, because the supervisor only
  tracks liveness. Both paths now resolve the directory the same way, and
  a `workdir` the undo removed is reported as a failed restart.

- `newgit undo` now brings back an uncommitted deletion of a tracked file.

  A checkpoint promises the worktree as it stood, uncommitted changes
  included, and its snapshot did record the deletion. But undo first reset
  hard to the checkpoint's commit, which put the file back, and then laid
  the snapshot over it, writing every path the snapshot had and removing
  none it lacked. The file came back with its committed content and a
  clean `git status`: work resumed from a state the instance was never in,
  and nothing showed it. With every tracked file deleted, undo failed
  instead, after the reset had already run. Checkpoints taken before this
  fix recorded their deletions too; undoing to one now restores them.


### Documentation

- The README covers using newgit from T3 Code: a fork gives each agent
  thread a newgit instance where T3 Code would add a Git worktree, and
  builds a desktop app that installs beside upstream T3 Code.

## [0.5.0] — 2026-10-22

The newgit remote (invite-only for now, at `newgit.dev`) and trackers all
the way down: every tracker is its own Git history, and a workspace's Git
is composed from the ones it binds. **Upgrading from 0.4.0 means
re-initializing the project**: see *Upgrading* in `CHANGELOG.md`.

### Added

- `newgit env [instance]` prints the environment `newgit run` and every
  hook get, each variable labeled with the declaration it came from, and
  `newgit action <resource>.<action> --dry-run` prints the rendered command
  and the directory it would run in, running nothing
  ([#80](https://github.com/Spheroman/newgit/issues/80)).

  Before this, the only way to learn which database a `[restore]` would
  drop was to run it. Both read the same code a real run uses — `env` is
  the one assembly every hook gets, `--dry-run` shares the run's
  resolution and refusals — so neither can describe something other than
  what would happen. Values are printed verbatim, credentials included,
  as `newgit run -- env` would; the help text says so.

- A workspace's `origin` now behaves like the project's real remote, and a
  plain `git push` from it publishes
  ([#96](https://github.com/Spheroman/newgit/issues/96)).

  A workspace is cloned from the store, so its `origin` was a local path: a
  push landed silently in the store (moving the branch ref behind
  checkpoint's back, reaching nothing anyone could see, and reporting
  success), `git pull` read the store, and `gh` could not find the
  repository. Agents kept rediscovering this and hand-rolling the two hops.

  Now `origin` fetches from the store's own `origin`, so `git pull` and
  `gh pr create` (no flags) work as in any clone. Pushes go to an outbox in
  `.newgit/local/publish/`, kept identical to the real remote, whose hook
  checkpoints the instance (reason `push` — `undo` cannot take a push back)
  and forwards the push. It succeeds exactly when the real remote accepted
  it; a store with no `origin` refuses it. Fast-forward and
  `--force-with-lease` are judged against the real remote, not the store.
  Existing workspaces pick up the route at their next `newgit checkpoint`.

- A view commit rebuilt because its reader cannot see every tracker carries
  a note naming the commit it stands for (`canonical <id>`). The Git endpoint
  serves each view's notes as `refs/notes/commits` — only for commits in that
  view — so `git log` shows them once a clone fetches `refs/notes/*`.

- `newgit login`, `logout`, and `whoami` sign in to a newgit remote with a
  GitHub account; `newgit remote add` creates the project there (pushing
  the store's history to its trunk) or joins it; `newgit members` manages
  access. In a linked project, `spawn` registers the instance — its own
  fork, and a token of its own kept in the workspace's `.git/` — and points
  the workspace's `origin` at trunk with newgit as Git's credential helper;
  `git push` publishes to the instance's fork on that token; `remove` takes
  the instance off the remote. A spawn whose registration fails still
  works locally and says so.

- `newgit tracker create <name> --code --path <path>` creates a code
  tracker: committed content with its own history, carved out of whichever
  tracker owned its paths (usually root) — every commit that touched them,
  marked with its source, so nothing is rewritten and composing still
  reproduces the project's history exactly. `newgit init` also writes the
  `project` tracker, which carries the committed parts of `.newgit/`.
  Capturing or tracking paths on a code tracker is refused: you commit to it
  with Git.

- The remote serves every project as an ordinary Git remote,
  `https://newgit.dev/<owner>/<name>.git`: `git clone` and `git pull` work
  with plain Git. Each branch is composed, in a per-project container running
  real `git` and `newgit`, from only the code trackers the caller may read,
  into a repository holding nothing else. Read-only; pushes go through a
  newgit workspace. The container sleeps when idle and restores from a
  snapshot.

- `.newgit/trackers/` is its own code tracker, `access`, created by `newgit
  init` with `maintainers = "owner"`: only the project's owner changes who may
  read what, while anyone who merges may change resources, scripts, and
  config (the `project` tracker).
- Trackers take an optional `maintainers` key — who may merge changes to them
  on a remote (`owner`, `project-devs`, `user:<login>`); by default, anyone
  who may read a tracker may change it. Changing a tracker's definition is
  changing the tracker that owns its file in `.newgit/trackers/`, so
  governance layers: a tracker can own other trackers' definitions, and only
  its maintainers redefine them. The remote and the Git endpoint read each
  definition from the tracker that owns its file.
- An instance reports whether it works, checked rather than remembered:
  `status` says `ready` or `not ready` with the reasons (a resource that did
  not come up, a tree it builds that is missing, a running resource's port
  not answering), and `spawn` ends with the same verdict. A resource's
  `prepare` runs quietly during `spawn`; a failure shows its last lines and
  its log.
- A checkpoint no longer aborts on a resource that is not up and whose
  capture fails: it goes ahead without it, marked degraded. `undo` rebuilds
  a resource's tree when it is missing, even if its inputs did not change.
- `spawn` says plainly that nothing leaves the machine until `git push`.

- `newgit merge [instance] [--into main]` merges an instance's committed work
  into a branch, tracker by tracker: unchanged trackers stay, one the branch
  has not moved on takes the instance's commit, one both sides changed gets a
  real Git merge in its own history, and any conflict moves nothing, naming
  the tracker and files. The branch's list then moves in one step — on the
  remote by compare-and-swap under a short merge lock, so a merge onto a list
  that moved meanwhile is refused. `newgit merge main` merges the project
  root. Merging is a person's decision: agent tokens cannot.
- Trackers carved, created, or redefined after `newgit remote add` reach the
  remote with the merge of the project root that made them: a new code
  tracker gets its repo then.

- A commit that changes code trackers with different audiences is refused
  where it would leave the machine (push, sync, merge): every tracker it
  changes keeps a copy of its message, so a message about work only some
  may read would land where everyone else reads it. Make one commit per
  audience. Merge commits are exempt.

- `newgit spawn <name> --private`: on the remote, only the instance's owner
  sees it — its record, checkpoints, and forks of the code trackers — until
  `newgit merge` publishes its work. Everyone else, the project's owner
  included, gets the same answer as for an instance that does not exist.
  Refused without a newgit remote, where a `git push` would publish it.
  `newgit status` shows `private`.

- Work is split into its trackers when it leaves the machine. Before a
  spawn, the project root's new commits reach each code tracker's
  `instances/main`; a `git push` or remote checkpoint sync splits the
  instance's commits into `instances/<instance>`. Split commits are signed
  as their author when `commit.gpgSign` is set and the author is you.
  Code trackers without history (one defined by hand, or `project`) are
  carved the first time they are needed.
- `newgit log <tracker> [instance]` shows one tracker's history.

- Every newgit token is recorded and can be revoked on its own, without
  logging anyone else out. Agents get their own tokens: they act as their
  owner and are named in the records they create, but cannot mint tokens,
  create projects, or manage members, default to 7 days, and can be pinned
  to one project.

- `.newgit/config.toml` accepts a `[remote]` section (`url`, optional
  `project`) naming the newgit remote a project syncs to. Nothing syncs yet.
  `url` must be https (plain http only for localhost), and
  `tracker create --storage remote` is refused when no `[remote]` is
  configured instead of quietly behaving as local.

- Registering an instance with the remote returns that workspace's own
  token, so nobody creates tokens by hand: it writes only its own instance
  (fork, checkpoints, head), renews itself while the instance exists, and
  is revoked when the instance is removed.

- The remote is invite-only. Only the GitHub logins in the Worker's
  `ALLOWED_LOGINS` secret (or anyone, with `*`) may log in or use a token,
  and the check runs on every request, so taking someone off the list shuts
  out the tokens they already hold. Unset admits no one: a deployment that
  forgot it is closed rather than open to every GitHub account.

- `remote/` is the newgit remote: a Cloudflare Worker with one Durable
  Object per project. It logs users in with GitHub's device flow, keeps
  members, instances, and checkpoints, forks an Artifacts repo per instance,
  and mints short-lived Git credentials by rule — trunk is never writable,
  a fork only by its owner, a tracker only by its audience as committed on
  trunk, and a per-user tracker only by that user.

- Tracker definitions take `role`, `priority`, `requires`, and `resources`.
  Trackers in a role are alternatives and may claim the same paths; a spawn
  binds one per role — the one `--use` names, else your own default
  (`[projects.<project>.roles]` in `~/.newgit/config.toml`), else the highest
  priority; a negative priority means only when asked. Binding a tracker
  binds what it `requires`, and a required tracker cannot be dropped.
  Resources a tracker brings bind only while it is bound, and start and stop
  with `newgit tracker use` and `drop`; `use` swaps a role's tracker.
  `spawn --use` / `--drop`.
- `{{bound.trackers}}` and `{{bound.paths}}` give a resource's commands what
  the instance binds, so an install can build only the bound members.
- `tracker list` is a tree by ownership, with each tracker's kind, role,
  priority, and maintainers; inside a workspace it marks what is bound.

- `newgit init` creates the `root` tracker, which owns everything no other
  tracker claims and starts with the project's own Git history (borrowed,
  not copied). Trackers may now nest — `apps` can hold `apps/electron` for
  another tracker — and a path belongs to its most specific owner, so
  capturing or restoring the outer tracker leaves the inner one's files
  alone. Two trackers claiming the identical path is still an error.
  Tracker definitions gain `secret` (default `true`: kept out of Git and
  captured from files, as every tracker has been); `newgit tracker list`
  shows root first.

- `newgit spawn <name> --from-checkpoint <instance>/<checkpoint>` spawns a
  checkpoint from the project's remote — anyone's, an agent's included — and
  it comes up as it was: source with its uncommitted work, shared tracker
  content, and resource state such as a database restored from its dump.
  Per-user trackers resolve to *your* content, never the author's, and a
  tracker you may not read is left out and named. In a linked project,
  every checkpoint syncs its source, its remote-stored tracker revs, and
  then its record to the remote; a failed sync is a warning, not a failed
  checkpoint. `spawn --json` prints what happened for agents.

- `newgit tracker use <tracker>` and `newgit tracker drop <tracker>` change
  what an instance binds. An instance's history holds every code tracker its
  user may read; dropping one takes its files out of the working tree (Git's
  sparse checkout) and its work out of the instance's scope — nothing splits
  into it, and a merge leaves it at the branch's commit — without a commit
  or any change to history. A drop is refused while the tracker has
  uncommitted work, unless `--stash` (brought back by `use`) or `--discard`
  (after a checkpoint, so `newgit undo` gets it back). Dropping a secret
  tracker captures its content first; `use` projects it back. `root`,
  `project`, and `access` cannot be dropped.


### Changed

- `newgit remove` deletes the instance's source branch when it holds
  nothing unique, and says why when it keeps it
  ([#82](https://github.com/Spheroman/newgit/issues/82)).

  It used to destroy the workspace and every resource it could tear down,
  then keep the one artifact that is cheap to keep and certain to bite: a
  leftover branch makes the next `spawn` of the same name adopt it instead
  of starting from `HEAD`, so every spawn/remove cycle needed a manual
  `git branch -D`. Now the branch goes when `spawn` created it, nothing has
  it checked out, and its tip is already on another branch, a tag, a
  remote-tracking ref, or the remote — asked live at removal, so a branch
  deleted from the remote since the last push counts as unique, and an
  unreachable remote keeps the branch. The check is repeated just before
  deleting. Otherwise it is kept and the output names the reason.
  `--keep-branch` and `--delete-branch` override; a forced delete prints the
  tip to recover from. `remove` also now says when the workspace held
  commits no checkpoint captured, since those go with the workspace. Instances spawned before this change
  never recorded whether `spawn` created their branch, so `remove` treats
  their branch as adopted and keeps it.

- The Git endpoint's container now stops after 10 minutes without a request,
  taking a snapshot first when anything was composed since the last one, so
  the next fetch restores everything it had. A fetch still streaming keeps
  it awake. Before, nothing stopped it explicitly, and a sleep could leave
  up to 10 minutes of composed views out of the snapshot.

- A `git push` from a workspace is refused when any commit it sends — on any
  branch or tag — adds or changes a file a secret tracker owns, naming the
  commit and file. A remote checkpoint sync checks the same, including its
  uncommitted-work commit. History from before newgit (root's `main`) is
  exempt, so an old committed `.env` never blocks a first push.
- Composing decides ownership among code trackers only, so history that once
  committed a now-secret file composes with its commit ids unchanged.
- CLI output says a tracker's `main` where it said "lane head".

- The remote stores code the way the machine does: one Artifacts repo per
  code tracker (`<owner>.<project>.t.<tracker>`), and an instance forks a
  tracker only the first time it changes it (`<owner>.<project>.i.<instance>.<tracker>`).
  The trunk repo and the whole-source fork per instance are gone; spawning
  forks nothing.
- Projects are `<owner>/<name>`, like GitHub. `newgit remote add` creates
  one, seeds each code tracker's repo, and creates branch `main`: a list of
  one commit per code tracker, which moves only through `newgit merge`.
  `--project <owner>/<name>` joins an existing one.
- A checkpoint sync pushes each changed tracker's branch and the
  checkpoint's commit to the instance's fork of it, and the record carries
  the checkpoint as lists. `spawn --from-checkpoint` composes those lists
  from the trackers you may read — the author's exact commits when you can
  read what they could. A `git push` on a remote succeeds only when that
  sync reached it.
- `newgit clone <owner>/<name>` composes a project's default branch from
  the code trackers you may read and sets newgit up around it.
- A workspace's `origin` is the project's Git endpoint,
  `https://newgit.dev/<owner>/<name>.git`; the credential helper answers it
  with the workspace's own token.

- The root tracker's history is copied into the tracker store at
  `newgit init` instead of borrowing the project repository's objects, so
  routine `git gc` in the project repository can never delete it.

- Each tracker's history is real Git history in a bare repo at
  `.newgit/local/trackers.git`, separate from the project repository:
  `main` (what new instances get), one branch per instance holding its
  captures as ordinary commits with a parent, author, and message, and pins
  keeping whatever bindings and checkpoints name. Capturing unchanged content
  makes no commit. Tracker content never reaches the project repository, a
  `git push --mirror`, or a workspace clone's object store, and
  `newgit cleanup` garbage-collects pruned revs at once.

  Upgrading: content under `.newgit/snapshots/` (and an interim
  `.newgit/local/lanes.git`) is not migrated. Re-seed each tracker
  (`newgit tracker capture <name> --from-store`) or re-capture it from an
  instance. Checkpoints recorded before the upgrade cannot restore tracker
  content.


### Fixed

- `checkpoint --verify` now says why a resource MISMATCHed — a `[restore]`
  that most likely landed elsewhere, or a `[checkpoint]` that is not
  deterministic — and shows what differs between the deposits
  ([#79](https://github.com/Spheroman/newgit/issues/79)).

  The two causes need opposite fixes, and two opaque revs could not tell
  them apart: a `pg_dump` checkpoint stamps every dump with a fresh random
  `\restrict` token, so a correct restore was reported as a failed one. On
  a mismatch, verify now checkpoints that resource once more with nothing
  restored in between: if that differs from the second ref, `[checkpoint]`
  is at fault; if it matches, `[restore]` most likely is. For
  `into_tracker` deposits it lists the changed files and line counts, shows
  the first changed lines with control characters escaped, and prints the
  `git diff --no-index` command for the rest — comparing `after` against
  the control when the checkpoint is the problem, since every line there is
  noise. The diff stays bounded for a dump of any size. A mismatch still
  fails the verify — this is evidence, not a looser check.

- `newgit export` now commits every path it ships, including `--include`d
  and public tracker paths
  ([#94](https://github.com/Spheroman/newgit/issues/94)).

  `tracker track` gitignores a lane's paths, and that `.gitignore` ships
  with the source tree, so the export's `git add -A` silently left every
  tracker file on disk and out of the repository. `git status` showed a
  clean tree and a clone of the export did not have the file, so the check
  the command recommends could not see what it had done. The export's
  commit is now exactly the plan, and the output lists any committed file
  the shipped `.gitignore` still matches — Git tracks it regardless, but a
  new file beside it would be ignored. The `.gitignore` itself is not
  rewritten: it is source, and it is what keeps a recipient's own copy of a
  withheld path out of their commits.

- A checkpoint no longer copies the workspace's tags into the store.

  Checkpoint fetches the workspace's state into the store, and a fetch
  follows tags by default, so every tag an agent created in a workspace
  appeared in the store as a side effect. Found because a `git push` of
  that tag was then refused: the store already had it.

- Composing keeps each tracker's own commit order whatever its dates say. It
  sorted every commit by committer date, so a real history with a rebased or
  cherry-picked commit (Express, 6,173 commits) recomposed into new commit
  ids, and the project root's first split then re-split almost all of it.
  Express now composes into itself, carved trackers included, in seconds.
- A split writes its notes in one batch instead of once per commit.

- Split commits record the commit they followed (`Newgit-Split-Parent`), and
  composing keeps that order over dates: work in two trackers made in the
  same second, or with dates running backwards, recomposes into the same
  commits — on the remote too, which has only the tracker commits.

- Restoring a deposit-only tracker (one that owns no paths, like
  `db-snapshots`) no longer writes its files into the workspace root; its
  content reaches resources through `{{state_ref}}` only. `newgit remote add`
  keeps a `[remote] project` already in `config.toml` instead of falling back
  to the local project name. Both found by running the flow against the live
  remote.

- From setting up a real monorepo (FareTable) with only the help text:
  - `newgit clone <project> <relative-dir>` works, and a clone that fails
    partway leaves nothing behind.
  - `tracker list` shows code trackers (`project`, and any carved ones).
  - A checkpoint says whether it reached the remote.
  - A spawn whose resources did not come up says so in its first line.
  - After a merge, the note about a project root with commits of its own
    names the branch and both ways forward.

- Fixes from the first live run against newgit.dev: `newgit remote add
  --project <name>` creates the project under that name; the credential
  helper answers for the Git endpoint when Git sends no path (as it does over
  HTTPS by default); and the endpoint's container authenticates its fetches
  from Artifacts.

- The bundled Postgres templates (`command-snapshot`,
  `command-snapshot-migrations`, `supabase`) now checkpoint to the same
  bytes for the same database, so `checkpoint --verify` can pass on them.

  Since 17.6 (and 16.10, 15.14, 14.19, 13.22), `pg_dump` wraps every plain
  dump in `\restrict` / `\unrestrict` with a fresh random key, so two dumps
  of an unchanged database never matched and every verify failed. The two
  host-`pg_dump` templates pass `--restrict-key=newgit`; `supabase`, whose
  CLI cannot forward that flag, swaps the random key for the same fixed one
  after dumping. A fixed key is a known key: it gives up pg_dump's defence
  against a hostile *server* injecting psql meta-commands into the dump.
  The server here is the instance's own local database, and the template
  comments say so. Existing resource files copied from these templates are
  not changed — add the flag by hand.

- Fixed data loss when a code tracker was carved after an instance's work
  had been split: carving now covers every branch and refreshes every view,
  and split refuses rather than start a tracker from an empty tree.
- Remote checkpoint sync checks every commit it uploads for secret files,
  merges included, like a push does; deleting a secret file is never
  refused; split no longer refuses secrets on its own, so one gate decides.
- The project root's tracker branches follow its own branch (the one
  checked out at `newgit init`), not a feature branch checked out there; an
  instance may not be named `main`.
- A split survives Git collecting a removed instance's commits, and does
  less repeated work (one fetch, the tip checked first, recomposing only
  when the set of code trackers changes).

- Revoking a newgit token, or removing an instance, also revokes the repo
  passwords it was handed, so Git access ends at once instead of lasting up
  to 15 minutes. Renewal is the exception: it leaves them to expire, so a
  push in flight is not cut off.

- Security fixes from review of the remote:
  - Tracker definitions decide every audience, so they are the project
    owner's: only the owner merges changes to the `project` tracker, adds
    code tracker repos, or writes the `project` tracker's repo. A member can
    no longer widen a secret tracker's audience by any route.
  - The Git endpoint composes a commit for a tracker only if that tracker's
    own repo has it; a branch naming another tracker's commit is left out,
    and cannot take the endpoint down with it.
  - Merge passwords require read access to a code tracker; a merge may move
    only the trackers it declared; branches are created by people only.
  - One secret gate before everything sent to the remote: sync, push,
    `newgit merge`, `newgit merge main`, and `remote add`'s first
    publication. A merge refuses when the instance's work did not sync.
- `spawn --from-checkpoint` works for a checkpoint taken after a merge.
- Merges and `remote add` can be retried after failing partway; `remote add`
  publishes `main` after merging the project root into it, so local merges
  are kept.
- A merge into `main` fast-forwards the project root when it can, so the next
  spawn starts from the merged work.
- `newgit clone` leaves trackers you cannot read absent rather than inventing
  their history; importing a checkpoint warns about trackers not defined
  here.

- Second-round review fixes:
  - The secret gate checks every commit actually pushed — each tracker's
    `main` too, which can hold work merged here that the project root's
    branch never had — against the history `newgit init` adopted (or a clone
    received), recorded once as `refs/newgit/adopted`.
  - A merge moves each tracker's branch on the remote only from the commit it
    started from (`--force-with-lease`), so a merge that stalled past its
    lock cannot rewind one that landed meanwhile. Merge write passwords last
    no longer than the lock and are cancelled when the merge finishes or a
    later merge takes an expired lock; the trackers a merge may move are
    recorded with its lock. A merge with nothing to move is refused.
  - A merge that succeeded is reported as succeeded even when bringing the
    project root along fails; `remote add` on an empty repository says to
    commit something first.

- A checkpoint records which trackers the instance did not bind, and `newgit
  undo` restores that too: trackers dropped since come back with their
  content, trackers used since are dropped again, and the resources they
  bring start or stop to match. `spawn --from-checkpoint` binds what the
  author's checkpoint bound.


### Documentation

- The README, AGENTS.md, the v1 spec, newgit-trackers.md, and
  newgit-cloudflare.md now describe binding as built: the `access` tracker
  and `maintainers`, roles and `priority` with personal defaults in
  `~/.newgit/config.toml`, `spawn --use/--drop`, `tracker use|drop
  [--stash|--discard]` by sparse checkout, readiness in `status`, the
  `tracker list` tree, and undo restoring the binding. Planned keys (`merge`,
  `production`) and role `depends_on` are marked as not wired yet.

- `newgit-cloudflare.md` now centers on `spawn --from-checkpoint` and
  trackers: spawning someone else's checkpoint resolves per-user trackers to
  your own content, and previews run in one sandbox per distinct
  environment, started from the checkpoint's container snapshot, a CI
  snapshot, or an environment snapshot keyed on resource versions.

- `newgit-cloudflare.md` plans the newgit remote on Cloudflare Workers and
  Artifacts: a fork per branch instance, a repo per tracker audience with
  server-enforced tokens, a Durable Object for binding records and
  checkpoints, and a server-side merge queue with overlap detection.

- `newgit-cloudflare.md`, `newgit-trackers.md`, the README, and the spec
  describe the remote as built: repos per tracker, branches as lists, the Git
  endpoint, `newgit clone`, and `newgit merge`.

- newgit-cloudflare.md is now the remote's design document rather than a
  competition plan: a status section of what is built and run live, an
  ordered *What's next* (GitHub export first, then previews and the web UI),
  and the spike results kept as *Measurements*. The repo topology and
  `spawn --from-checkpoint` sections describe per-tracker repos as built.

- The README, the v1 spec, `newgit reference`, and the architecture and
  Cloudflare docs now describe the tracker model the code implements: `root`
  owns `.` and `project` owns `.newgit`; ownership nests, with the most
  specific claim winning; `secret` and code trackers (`tracker create --code
  --path`) replace the implicit `source` tracker; and the remote commands
  (`login`, `remote add`, `members`, `spawn --from-checkpoint`) are listed.
  The `--from-checkpoint` help no longer points at a `checkpoints --remote`
  flag that does not exist.

## [0.4.0] — 2026-09-16

### Added

- A `supabase` resource template, and every template now states the host
  tools it assumes
  ([#90](https://github.com/Spheroman/newgit/issues/90)).

  `command-snapshot-migrations` named its audience in its own opening
  comment — "Rails, Prisma, **Supabase**, and anything else that runs
  `migrate`" — and then handed out `createdb`, `pg_dump` and `psql`. A
  Supabase user has none of those against their database by construction:
  it lives in a container the CLI manages. Same for a Prisma user on Docker
  Compose, which is most of them. The template read as "ready to adapt"
  when it was really "ready to adapt *if your database is on the host*".

  `newgit resource add db --template supabase` now writes the shape that
  actually works: `supabase start` for `prepare`, `supabase db dump --local
  --data-only` into the `db-snapshots` lane for `checkpoint`, reset →
  truncate-what-this-role-may-truncate → load for `restore`, and `supabase
  stop --no-backup` for `cleanup` — every database command through the CLI
  or `docker exec`, never a host binary. It renders `project_id` and all six
  published ports out of the committed `supabase/config.toml`, and carries a
  commented-out `additional_redirect_urls` render for the Expo case, which
  is a data edge rather than a `depends_on`.

  `project_id` is the load-bearing line: it scopes every container name and
  Docker volume, so without a per-instance value the second instance's
  `supabase start` adopts the first instance's stack. It ships as an
  explicit `EDIT_ME_PROJECT`, so an unedited template refuses at spawn
  naming the file and the string rather than quietly sharing a database
  between branches.

  The smaller half of the same report: all seven templates now open with a
  `HOST TOOLS:` line. The two host-Postgres ones say so and point at
  `supabase` or at putting `docker exec` in front; `install` admits that its
  `key_command` ships as `node -v` and is wrong for a uv or Cargo project;
  `external` says `cloudctl` is not a real program. A test asserts every
  template carries the line, and another renders `supabase` against the
  config `supabase init` actually writes, so the `find` strings cannot ship
  broken.

- Add `newgit ports --check`: detect a host port that is listening but that
  no binding record claims
  ([#92](https://github.com/Spheroman/newgit/issues/92)).

  The inverse of `render --check`: it runs `lsof -nP -iTCP -sTCP:LISTEN`
  and compares what is actually listening against every binding record,
  flagging a listening port inside any resource's allocatable range that
  nothing claims. It deliberately does not fix the allocator's own bind
  probe (`bindable` still only tries `127.0.0.1`, the root cause of #93)
  — it is a cheap, inspectable mitigation that would have caught #93's
  port collision after the fact, without requiring the probe to get
  Docker-published ports right.

  Attribution to an instance is honest rather than complete: a listening
  port is named to an instance only when that instance's name or
  workspace path genuinely appears in the owning process's command line.
  On macOS, a Docker Desktop container's published port is fronted by
  Docker's own VM proxy, whose command line says nothing about the
  instance that owns the container, so most Docker-caused conflicts come
  back unattributed rather than guessed — an honest "something claims
  this port and it isn't newgit" is still the useful finding.

  Bare `newgit ports` (no `--check`) lists every instance's claimed ports
  straight from its binding record — no probing, no `lsof` dependency —
  since that ledger is useful on its own and costs nothing to print.

  If `lsof` is missing or exits with anything other than "nothing
  found", `--check` says it could not check rather than silently
  reporting a clean pass.


### Fixed

- Two papercuts from one integration session
  ([#84](https://github.com/Spheroman/newgit/issues/84)).

  `cleanup` said `Nothing to clean up.` and then, immediately below,
  described eleven snapshot revs it had kept and why. The retention report
  is the useful part — it is what sends you to `--purge-archived` — but the
  line above it made the whole output read as self-contradictory, and the
  first reaction is to reread it working out which half is wrong. It now
  says `Nothing to remove.`, which is the true and narrower claim and
  leaves the retention report free to explain what was kept.

  `spawn` now says when it is continuing checkpoint numbering left behind
  by a previous instance of the same name. Numbering is per name and
  `remove` archives the binding record without deleting the checkpoint
  files, so a name that has been removed and spawned again takes its first
  checkpoint as `ckpt_004` on an instance thirty seconds old — the one
  thing that carried over when the workspace, containers and volumes were
  all destroyed and rebuilt, which is the opposite of what the rest of
  `remove` implies.

  Restarting at `ckpt_001` was the other option and is not available: the
  old `ckpt_001.toml` is still on disk — releasing it is exactly what
  `newgit cleanup --purge-archived` is for — and reusing the id would make
  it ambiguous which of two checkpoints `newgit undo ckpt_001` meant. So
  `spawn` states it at the moment it becomes true instead, on a `history:`
  line naming the count, the id the first checkpoint here will get, and the
  command that releases them.

- `spawn` now exits non-zero when any resource fails to bind
  ([#86](https://github.com/Spheroman/newgit/issues/86)).

  A resource that printed `export: FAILED`, `render: FAILED`, `prepare:
  FAILED`, or `prepare: BLOCKED by ...` left `spawn` exiting 0 anyway, so a
  script, CI job, or agent driving newgit could not tell "instance ready"
  from "instance spawned but broken" without parsing the human-readable
  summary. `BLOCKED` counts alongside `FAILED`: a blocked `prepare` never
  ran, so that resource is no more usable than one whose `prepare` ran and
  failed. This mirrors #11's fix for `undo` ("a failed undo reports as
  Restored") and uses the same plain `exit 1` `undo` and `checkpoint
  --verify` already use, rather than a distinct code, so `if newgit spawn
  x; then ...` keeps working. The instance and its record are still
  created — exit 1 reports the state, it does not roll the spawn back —
  and the closing line now says so explicitly: `spawned with failures: 1
  of 4 resources did not bind.`

- The port allocator's bind probe no longer reports a port bindable when a
  Docker-published container port already holds it
  ([#93](https://github.com/Spheroman/newgit/issues/93)).

  It checked only `127.0.0.1`, using `std::net::TcpListener::bind`, which
  sets `SO_REUSEADDR` on by default on Unix. That let a loopback bind
  succeed even when something else — a Docker Desktop container publishing
  to `0.0.0.0`, in the reported case — already held the port on the
  wildcard address, so `spawn` allocated a port it could not actually use.
  Since allocation is permanent for the life of an instance, the bad value
  did not get retried; it just failed later, inside a resource's prepare
  log. The probe now binds, with `SO_REUSEADDR` off, on `127.0.0.1`,
  `0.0.0.0`, `::1`, and `::`, and only reports a port bindable if all four
  are free. An address family the OS doesn't support at all is skipped
  rather than counted against the port. If the probe can't positively
  confirm an address is free, it now declines to promise the port rather
  than claim it.

  A side effect worth knowing about: the old loopback-only probe, with
  `SO_REUSEADDR` on, could not see another process's simultaneous probe of
  the same port either — so two `newgit spawn` runs racing each other could
  both be handed the same port. The stricter probe closes that gap too:
  concurrent allocators now correctly see each other's in-flight claims and
  scan past them instead of colliding.


### Documentation

- The reference now states that `[checkpoint]`, `[restore]` and `[cleanup]`
  receive the full command environment
  ([#77](https://github.com/Spheroman/newgit/issues/77)).

  They always did — all three call the same `assemble_env` an action does —
  but every mention of the environment was scoped to "`newgit run` and
  actions", and the same section states plainly that those three hooks *are
  not actions*. Between those two facts the reference could not answer
  whether a `[restore]` sees `[exports]` and `[ports.<name>] env`, and the
  omission read as deliberate because the neighbouring template-variable
  scope table resolves the same question for `{{...}}`.

  That is the worst hook to leave unanswered. A `[restore]` that resets a
  database picks *which* database from the port newgit allocated; without
  that variable it falls back to the committed default, which is usually the
  developer's shared local database. Discovering the contract empirically
  means running a destructive command against live state to see what
  survives.

  There is now an environment scope table beside the template-variable one,
  `[exports]` and `[ports.<name>] env` say "every command newgit runs"
  instead of naming actions, and the "not actions" sentence says what it is
  and is not a claim about — `workdir` and invocability, not the
  environment. Unlike `{{...}}`, the environment does not vary by site:
  there is one environment and every command gets all of it. A new test
  (`tests/hook_env.rs`) asserts all three hooks see another resource's
  export, a port `env`, and `NEWGIT_*`, so the documented claim cannot drift
  from the code.

- The reference now states that a resource's ports and `[exports]` are bound
  before its own `prepare` runs
  ([#78](https://github.com/Spheroman/newgit/issues/78)).

  `[exports]` was "rendered once at `spawn`" and `prepare` "runs on its own —
  at `spawn`". Both at `spawn`, with no order between them, which is not
  enough to write a `prepare` against. The within-resource order is now
  written down — allocate `[ports]`, render `[exports]`, apply `[[render]]`,
  then run `prepare` — along with the fact that a dependency's exports were
  bound an iteration earlier, in dependency order.

  The failure from guessing wrong is quiet: a `prepare` that brings up a
  Compose stack named by its own `{{branch.slug}}` export, run before that
  export existed, brings up the *shared* stack under the committed default
  name. No error, correct-looking output, wrong cluster. The workaround —
  restating the value inline in every hook that needs it — is the
  duplication this reference warns against elsewhere, and is no longer
  necessary. `tests/bind_order_and_stop_name.rs` pins both halves.

- The reference no longer contradicts itself about whether `stop` is a
  reserved action name
  ([#81](https://github.com/Spheroman/newgit/issues/81)).

  The `signal` row documented a default of "`term` for a `stop` action", and
  three paragraphs later the same section said action names carry no
  meaning — "`start`/`stop` are a convention, made real by `long_running`
  and `signal`, not by the names". The code agrees with the first one:
  `stop_signal()` is a literal `actions.get("stop")`.

  `stop` is now documented as the one name newgit reads, and for exactly one
  purpose: when newgit stops a supervised process on its own — during
  `remove`, `undo`, and `resource remove --force` — it signals, and reads
  `[actions.stop] signal` to choose which signal, defaulting to `term`.

  The part the issue could not determine from outside is now stated and
  tested. An action named `stop` **with a `command`** is accepted and is an
  ordinary action: invoking it runs the command like any other. But newgit's
  own stops always signal and never run an action's command, so such an
  action is never reached that way and the resource gets a bare `term`. A
  teardown that is a command rather than a signal (`docker compose stop`)
  belongs in `[cleanup]`, which newgit does run.

## [0.3.0] — 2026-09-15

### Added

- `newgit resource remove <name>` and `newgit tracker remove <name>` — the
  inverse of `resource add`/`tracker create` that only existed as `rm
  .newgit/resources/<name>.toml` before. Deletion by hand was predictable —
  definitions are plain TOML named by filename — but it was the one place in
  the tool where you had to reach around the CLI, and it turned up in the
  first session cleaning up a definition `resource add --template pnpm` had
  created on its own.

  Both refuse rather than leave damage behind, in the voice `newgit remove`
  already set: `resource remove` refuses if another resource still names it
  in `depends_on` (naming every dependent, and `--force` does not override
  this — a broken graph is never what you wanted) and refuses if a live
  instance still has it bound unless `--force` is given, in which case it
  drops the binding and releases the ports (there is no other ledger; a port
  is free the moment nothing claims it). `tracker remove` refuses the same
  way for a bound instance, refuses outright for `source` (Git/jj owns that
  history, there is no file to remove), and undoes exactly what `tracker
  track` did — the store's `.gitignore` block and each workspace's
  `.git/info/exclude` entries. Captured content under `.newgit/snapshots/`
  is left in place; it is now unreferenced, and a `newgit cleanup` bug fix
  alongside this (lane heads were pinned as roots forever, even after their
  tracker's definition was deleted) means that cleanup now actually reclaims
  it once nothing else — a checkpoint that captured it — still pins a rev.

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

- Resources can set `workdir`, overridable per action, so a monorepo
  definition does not have to start every command with the same `cd`.

  A resource whose real work lives at `packages/db/supabase` used to repeat
  `cd packages/db/supabase && ...` on `prepare`, `stop`, `checkpoint`,
  `restore`, and `cleanup` alike — seven lines, one prefix, and a `&&` that
  silently swallows a failed `cd` because the command after it still runs.
  `workdir = "packages/db/supabase"` at the top of the resource says it once;
  newgit spawns the command there directly (`Command::current_dir`, not a
  shell prefix), so there is no `&&` left to bind wrong. An action can set
  its own `workdir` to replace it for just that one command.

  It only ever changes where a *command* runs. `[identity].paths`,
  `[checkpoint].paths`, and `[[render]].path` stay workspace-root-relative
  regardless — those are content paths, and a definition that had to track
  two roots at once would be worse than the `cd` it replaces.

- `newgit status` reports when an instance's base has moved —
  `ok, main +2` — instead of staying silent while a rebase gets
  cheaper to do by the day
  ([#38](https://github.com/Spheroman/newgit/issues/38)).

  The binding record now keeps the branch `spawn --from` used (or
  whatever `HEAD` named when `--from` was omitted) alongside the
  revision it pointed at, fixed at spawn time and never touched by
  `checkpoint`. `status` compares that fixed point against the base
  branch's current tip in the store repo, computed fresh — never
  cached — on every call, so the count is exactly as current as the
  store's last fetch of upstream and never a stale answer dressed up
  as a fresh one. An instance spawned onto a branch that already
  existed has no recorded base and reports none, rather than guessing
  one; same when the base branch has since been deleted.

- `[exports]` may compose a dependency's exports
  ([#40](https://github.com/Spheroman/newgit/issues/40)).

  A `[[render]]` could already see every export bound so far, so a *file*
  could carry another resource's URL while the resource itself could not
  publish one — the wrong way round, since `[exports]` is what produces those
  values in the first place. Bindings happen in dependency order, so the
  values were already there; nothing passed them.

  ```toml
  depends_on = ["supabase"]

  [exports]
  SUPABASE_FUNCTIONS_URL = "{{exports.SUPABASE_API_URL}}/functions/v1"
  ```

  This is the expressible form of what #40 tried to write as
  `{{ports.supabase.api}}`: there is no syntax for another resource's ports,
  and now there does not need to be.

- `newgit undo --only <resource>` restores one resource and leaves source,
  tracker content, and every other resource alone
  ([#41](https://github.com/Spheroman/newgit/issues/41)).

  Getting one resource's restore command right takes a few attempts, and
  rewinding six other resources each cycle is pure cost. A partial undo is not
  a snapshot the instance was ever in, so it never claims to be one:

  ```
  Restored `supabase` of `newgit-smoke` from ckpt_004
    source and tracker content left as they were (--only)
  ```

  It still takes a safety checkpoint first — it is still destructive — and it
  still stops and restarts the resources it touches, but only those.

- Checkpoint output says when a resource's `[restore]` has never completed
  on the instance, and `newgit checkpoint --verify <instance>` can prove it
  on demand
  ([#42](https://github.com/Spheroman/newgit/issues/42)).

  `[checkpoint]` runs the day it is written; `[restore]` runs the day it is
  needed, which by definition is the day the instance is already in
  trouble. Every checkpoint in between looked identical whether or not the
  restore behind it had ever actually worked. A resource whose `[restore]`
  is `command` or `recompute` and has never completed successfully on this
  instance now gets `— restore never exercised` on its checkpoint line.

  `newgit checkpoint --verify <instance>` proves it early instead of
  leaving that for the day of the real rollback: checkpoint, restore with a
  real `undo`, checkpoint again, and compare the two runs' state refs.
  Destructive and expensive on purpose — it stops and restarts whatever the
  instance's resources run, which is exactly why it is a separate,
  explicitly named command rather than something `checkpoint` does on its
  own, and why it prints what it is about to do before doing it.

  "Completed" and "verified" are deliberately different claims: an ordinary
  undo marks `[restore]` as proven the moment its command exits `0`, even
  though that alone does not confirm the resource landed on the state the
  checkpoint recorded. `--verify`'s state-ref comparison is the stronger
  check, and it can fail even when the weaker one would have passed.

- The **install store**: `[identity] produces` names the tree an install
  builds, and the second instance of a given lockfile is cloned from the
  first instead of installing
  ([#50](https://github.com/Spheroman/newgit/issues/50)).

  Every instance already got its own `node_modules`, and instances at
  *different* lockfiles still must. But instances at the *same* lockfile were
  rebuilding a tree byte-identical to one already on the machine — on a real
  Expo monorepo, ~90 s and 9.0 GB, every time. Declaring what the install
  produces makes that a clone:

  ```toml
  [identity]
  paths       = ["package-lock.json", "package.json"]
  produces    = ["node_modules"]
  key_command = "node -v && uname -sm"
  ```

  ```
  resource:  `deps` prepare: ok
             install store: stored as 1264badb0621 (copy-on-write)

  resource:  `deps` prepare: not needed
             install store: filled from 1264badb0621 (copy-on-write)
  ```

  The key is the content of `paths`, `key_command`'s stdout, and the
  definition file itself — the command that builds a tree is as much an input
  as the lockfile it reads, so editing `prepare` rebuilds rather than reusing
  what the old command built.

  `key_command` is the half a lockfile cannot supply: its hash describes what
  was asked for, not what gets built, and install scripts compile against a
  platform and toolchain it never sees. newgit does not guess at that list —
  what a tree depends on beyond its lockfile is ecosystem knowledge the
  definition has and newgit does not.

  Clones are **copy-on-write, never hardlinks**, and that is correctness
  rather than performance. `node_modules` is not read-only after install:
  native builds write into it (693 files of CMake output under
  `react-native-reanimated/android/.cxx/` on the project this came from), so
  under hardlinks one instance's Android build would rewrite every other
  instance's tree — the exact corruption the design exists to prevent,
  arriving by a route hash-keying alone does not cover. Where the filesystem
  cannot clone, newgit takes a full copy and says so: same behaviour
  everywhere, worse performance, and on such a filesystem an entry costs a
  whole tree on disk rather than almost nothing.

  Copy-on-write does not help if the poison is already in the shared content,
  so a tree is scanned before it is published and declined if it names the
  workspace it was built in, with the file named. That resource keeps
  installing per instance; nothing fails. The store never fails a spawn at
  all — every way it can go wrong degrades to the install that would have
  happened anyway, and says which.

  An undo is filled from the store as well — that rebuild is the expensive
  one, a full reinstall in the middle of an operation someone is waiting on.
  The tree being rewound away from is moved aside and discarded only once the
  clone lands, so a copy that fails partway through leaves the instance with
  the stale tree rather than none. `newgit undo --force-recompute` bypasses
  the store and **drops the entry**: that flag means the identity is not to be
  trusted to describe the tree, a cache keyed on the identity is under the
  same suspicion, and this is the way out if an entry is ever wrong.

  Declared `produces` paths join tracker-owned paths in each workspace's
  `.git/info/exclude`. Derived content is not source, and without the rule a
  project with no `node_modules` entry of its own would have its whole install
  swept into source history by the `git add -A` a checkpoint runs.

  `newgit cleanup` prunes entries no live instance's identity keys to, plus
  anything an interrupted publish left half-copied. The tree is rebuildable
  from the inputs that key it, so the worst a wrong eviction costs is an
  install. Reachability is recomputed rather than recorded — an entry is kept
  exactly when a spawn of that instance would have found it — and gating is
  per resource, so one instance that cannot be keyed does not disable the
  sweep for the whole project. `--dry-run` does not run a `key_command` at
  all: a dry run observes, and spawning a user-supplied shell is acting.

  `newgit action <resource>.prepare` always runs the command. The store
  stands in for a *spawn*, never for a command someone asked for by name —
  but it publishes what that command built.

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

- Added a `command-snapshot-migrations` resource template, and named the
  assumption `command-snapshot` was quietly making
  ([#57](https://github.com/Spheroman/newgit/issues/57)).

  `command-snapshot`'s restore is `dropdb`/`createdb` then load, which only
  works when the dump is the whole database — a full dump loaded into a
  target with nothing in it to collide with. That shape is unavailable the
  moment the schema comes from migrations instead (Rails, Prisma,
  Supabase): the database can't be dropped, because recreating it means
  replaying every migration, so the restore becomes reset, then load data
  over rows the migrations already inserted. The dump has to be
  `--data-only`, and the target has to be emptied first — but only of what
  the role running the restore can actually truncate, since the app role is
  rarely the superuser and a table the dump could not read is not one the
  restore has any business emptying either.

  These are two restore strategies, not two settings on one command, so
  they're two templates: `command-snapshot` now says what it assumes in a
  comment and points at the new one, and `command-snapshot-migrations`
  ships the reset-empty-load shape, privilege filter included. Both deposit
  into the same `db-snapshots` lane.

  `definitions.md`'s `[restore]` section also gained one sentence: a
  restore command runs against whatever state its own reset left behind,
  including rows a migration step already inserted, so it is not a fresh
  database unless the command made one.

- Added an `install` resource template: a generic dependency-install
  starter with no package manager assumed
  ([#60](https://github.com/Spheroman/newgit/issues/60)).

  `pnpm` was the only install starter, so an npm (or uv, or Cargo) project
  had to begin from a file that was wrong line by line: `depends_on`, the
  lockfile name, and the install command all needed changing, and
  instantiating it also created a `pnpm-store` resource the user then had
  to delete in a specific order. What survived unedited was exactly the
  three things a user could not have guessed — `ownership`, `[checkpoint]
  mode = "hash"`, `[restore] mode = "recompute"` — which was the whole
  reason to start from a template at all.

  Rather than one more named template per package manager (`npm`, `uv`,
  `cargo`, ...), `install` parameterizes nothing: no `depends_on`, no
  companion resource, and its two variable lines — `[identity] paths` and
  the `prepare` command — are `EDIT ME` placeholders instead of a guess
  that happens to be wrong for whichever manager it wasn't written for.
  `pnpm` remains the worked example, since it is also the one that wires up
  a shared, content-addressed store; the reference's *Installs* section now
  points newcomers at `install` when pnpm isn't their package manager.

  `resource add --template pnpm` also now says that its companion
  `pnpm-store` must be removed *after* the resource that depends on it,
  since `resource remove` enforces the order but previously only explained
  why when it refused.

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


### Changed

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

- `depends_on` means lifecycle again. Needing another resource's *value* is
  inferred from `{{exports.<name>}}` instead of declared, and orders binding
  without touching teardown
  ([#43](https://github.com/Spheroman/newgit/issues/43)).

  One key was driving two different claims. A `[[render]]` substituting
  another resource's URL into a config file needs that resource *bound*
  before it renders and nothing more — but the only key that produced that
  ordering also reversed into cleanup, so a pure data edge had to be written
  as a lifecycle edge and silently acquired teardown semantics it never asked
  for. The Supabase stack does not need the Expo dev server running, started,
  or ever used; it needs one string out of it. The graph asserted otherwise
  because there was no way to say the weaker thing.

  There still isn't a way to *say* it, and that is the fix: the template is
  already the statement. `{{exports.EXPO_URL}}` names what it needs, so the
  edge is read from it rather than restated:

  ```toml
  # supabase.toml — no depends_on
  [[render]]
  path = "packages/db/supabase/config.toml"
  replace = [
    { find = 'additional_redirect_urls = ["exp://127.0.0.1:8081"]',
      with = 'additional_redirect_urls = ["{{exports.EXPO_URL}}"]' },
  ]
  ```

  `web` binds first and the file renders correctly, with nothing declared in
  either direction. The graph now keeps two orders: bind order (`depends_on`
  plus data edges) for preparing, rendering, env assembly, and restore;
  lifecycle order (`depends_on` alone) reversed for checkpoint and cleanup.

  Inference costs legibility, so `newgit resource list` prints what it found,
  naming the export that caused each edge — an edge nobody wrote down has to
  be able to explain itself:

  ```
  Reads exports from (inferred from `{{exports.*}}`):
    supabase reads web (EXPO_URL)
    these order binding only — they say nothing about teardown
  ```

  Only bind-time consumers count: `[exports]` values and `[[render]]`
  replacements. `[cleanup]` and `[checkpoint]` may use `{{exports.*}}` too,
  but they read a binding record that is already complete, so there is
  nothing left to order. A name no resource exports is not an edge — it is a
  template that will not resolve, which `[exports]` and `[[render]]` already
  refuse with a better message. A resource referring to its own exports is
  the documented sibling case, not an edge to itself. A cycle through data
  edges alone is a real cycle and is reported like any other.

  This is what made the export-name rule below worth having first: reading an
  edge out of `{{exports.EXPO_URL}}` only works if exactly one resource can
  own that name.

- An environment variable name may be declared only once across the project;
  a second claim is a graph problem
  ([#43](https://github.com/Spheroman/newgit/issues/43)).

  Three things declare a name — an `[exports]` key, an action's `captures`
  entry, and a port's `env` — and the command environment used to be
  assembled by layering them, exports in dependency order with port `env`
  vars over the top. So a name claimed twice resolved to whichever
  declaration happened to come last, and the loser was simply *absent* from
  the process that needed it, with nothing anywhere saying why. Both
  declarations look correct in their own file; the fault only exists in the
  union, which is not a thing you can read.

  Nothing wanted that behavior. It was never an override feature, just what
  fell out of building a map — and it was not even consistent, since port
  `env` vars beat exports regardless of dependency order while exports beat
  each other according to it. The set of names is fully known from the
  definitions, so the collision is now reported when the graph loads:

  ```
  environment variable `EXPO_URL` is declared more than once (`metro`
  [exports], `supabase` [exports]); a name may have only one owner — rename
  all but one, and compose it elsewhere with `{{exports.EXPO_URL}}`
  ```

  It is gated like any other graph problem — warned by the commands that
  *build* the graph, refused by `spawn`, `run`, `action`, `checkpoint`, and
  `undo` — so a project cannot be bricked by one, and `newgit resource list`
  still tells you where it is.

  The starter templates had to change with it. Adding the same template twice
  — a web and an api — is the canonical setup, and shipping conventional
  names meant the second `newgit resource add --template process` claimed the
  `PORT` and `APP_URL` the first already owned and refused the whole graph.
  That is newgit's own template breaking the project, not a user mistake, so
  templates now name their variables after the resource: `resource add web
  --template process` writes `WEB_PORT` and `WEB_URL`. The generated file
  says to rename them if you have one service and your tool insists on
  `PORT`. Two services in one project never could both publish it — every
  resource's environment lands in one process environment — so the
  conventional name was a promise templates could not keep.

  One overlap is still allowed, because it has one owner: a `captures` entry
  naming its own resource's `[exports]` key. The export states the value the
  definition knows up front and the action overwrites it with the one that
  did not exist until it ran.

  `NEWGIT_BRANCH` and `NEWGIT_WORKSPACE` are reserved for the same reason.
  newgit sets them last for every command it runs, so a resource declaring
  one could never reach the process — reported against the single claimant
  rather than passed over in silence.

- `[identity]` is now the single declaration of what a resource is derived
  from, and `[checkpoint] paths` is gone
  ([#45](https://github.com/Spheroman/newgit/issues/45),
  [#46](https://github.com/Spheroman/newgit/issues/46),
  [#41](https://github.com/Spheroman/newgit/issues/41)).

  The reference described these three keys as one mechanism — identity feeds
  the checkpoint, the checkpoint feeds the restore — and the mechanism did not
  exist. `[identity].paths` was read in exactly one place, to put the word
  `identity` in a column of `newgit resource list`. `hash:<rev>` was written at
  checkpoint and parsed back nowhere. `recompute` re-ran its action without
  consulting either. Definitions worked only because their authors dutifully
  typed the same path list into two blocks; diverge them and nothing said so.
  The shipped `pnpm` template duplicated the list too.

  ```diff
  [identity]
  paths = ["package.json", "pnpm-lock.yaml"]

  [checkpoint]
  mode = "hash"
  -paths = ["package.json", "pnpm-lock.yaml"]
  ```

  `mode = "hash"` now hashes `[identity] paths` and has no path list of its
  own; it fails to load without an `[identity]` to hash. Identity paths are
  validated the way tracker paths always were — workspace-relative, no `..`,
  no reaching into `.git` or `.newgit`. Previously `paths = ["/etc/passwd",
  "../../escape"]` loaded clean.

  And `recompute` consults the hash, which is the point of recording it:

  ```
  resource: deps recompute(prepare) skipped: identity unchanged
  ```

  A monorepo `npm ci` is around ninety seconds, and three debugging cycles on
  an unrelated resource's restore command used to cost three of them. The
  comparison is between the checkpoint and the *pre-undo* state, not the
  workspace as it stands when the resource is reached — undo restores source
  first, so hashing at that point would compare the checkpoint against itself
  and skip every time, including the one case that matters: a lockfile that
  moved after the checkpoint and has just been rewound underneath a tree built
  from the newer one.

  Identity describes the inputs, not the tree, so the repair path stays
  reachable: `newgit undo --force-recompute` rebuilds regardless.


### Removed

- `kind` is gone from resource definitions
  ([#27](https://github.com/Spheroman/newgit/issues/27)). It was required on
  every resource and read nowhere — the only code that touched it was the
  parser, the struct, and the `KIND` column in `newgit resource list`. A
  required field with no behavior still reads as if it has one: a Supabase
  stack got `kind = "process"` because it runs a long-lived service, while
  two genuinely supervised resources in the same project also said
  `process` — one string, two meanings, and no rule to pick it by. Making
  `kind` real (defaults keyed off its value) was considered and rejected: that
  would let the field and the sections below it disagree, which is exactly
  how the Supabase resource got mislabelled. What a resource does was always
  fully described by its sections; `kind` never added anything to check
  against. A leftover `kind = "..."` line in an existing definition is
  silently ignored — the TOML parser already ignores unknown fields, so
  nothing had to change to make old files keep loading.

  `newgit resource list`'s `KIND` column is replaced with `PROFILE`, built
  from facts newgit can recompute from the TOML rather than a label someone
  wrote down once: `long-running` (a start action that doesn't exit),
  `ports`, `identity`, `render`, and `checkpoint:hash`/`command`/`external`.
  Unlike `kind`, this cannot drift from what the resource actually does,
  because it isn't stored anywhere to drift from.


### Fixed

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

- `status` no longer reports states that are not true (#28), in three ways.

  A resource with an unrelated `long_running` action it never started — say,
  a stray `functions` action beside the `prepare` a Supabase resource
  actually uses — showed up as `stopped` on the strength of the declaration
  alone; the column now asks the supervisor whether *something it started*
  has since died, which is a fact newgit can actually know, rather than "does
  this resource's definition mention a long-running action at all." Telling
  those apart means a pid file has to survive being stopped — but leaving a
  raw pid sitting on disk indefinitely would reopen a worse lie once the OS
  recycles that number onto some unrelated process, so a stopped process now
  retires its file to the literal `stopped` instead of leaving the number
  behind, and `newgit cleanup`'s pid housekeeping does the same to a pid file
  whose process died on its own — rewriting it in place rather than deleting
  it, since deleting it is exactly what would have made `newgit cleanup`
  itself erase the fact `status` depends on.

  `blocked` used to be written into the binding record, which meant a
  resource with no `prepare` of its own — like an `admin`/`mobile` dev server
  waiting on a Supabase stack — stayed `blocked` forever once its dependency
  recovered, because nothing ever ran to recompute it. Blocked-ness is
  derived at read time from `depends_on` instead, so it clears the moment the
  blocker does, and `status` names what's blocking (`admin:blocked(supabase)`)
  rather than just saying so. A resource with no `prepare` at all goes
  further: it is `ready` from the moment it is bound, blocked dependency or
  not, because there was never a command to withhold from it in the first
  place — the same rule that makes a resource with a real `prepare` correctly
  stay `pending` while it waits.

  `status` is the command you run when you're already confused about what
  newgit thinks is true; it should not make that worse.

- An unresolved `{{...}}` in `[exports]` refuses at spawn instead of being
  stored and shipped as a literal
  ([#40](https://github.com/Spheroman/newgit/issues/40)).

  Everywhere else an unknown placeholder renders verbatim, so the mistake is
  visible to whoever typed it. An export is the exception, for the same reason
  `[cleanup]` and `[[render]]` already refuse: it is rendered once, written
  into the binding record, and handed to every later action and `newgit run`
  as an environment variable. A bad command fails in front of the person who
  wrote it; a bad export surfaces in a different process, at whatever hour
  something first dials a host named `{{ports`.

  ```
  resource:  `functions` export: FAILED — resource `functions` leaves exports
  unresolved: `SUPABASE_FUNCTIONS_URL` ({{ports.supabase.api}}); ...
  ```

  Nothing that resource exports is stored, not just the value that failed: an
  absent environment variable is something downstream can detect, a malformed
  URL is not, and a binding that publishes half an environment is the same
  failure one variable further down — `newgit run` and every dependent's
  actions read a binding's exports without asking what status it holds. Every
  unresolved export is named at once, so fixing the first does not just reveal
  the second on the next spawn. Export failures are reported apart from render
  failures, because they are different mistakes in different parts of the
  definition.

  An export may compose a *sibling* as well as a dependency's export
  (`HEALTH_URL = "{{exports.BASE_URL}}/health"`). Composing the resource next
  door while the key two lines up was refused would have been a rule nobody
  could guess — and the refusal named a key that was defined right there. The
  table is a map with no declaration order to lean on (`HEALTH_URL` sorts
  first), so exports resolve to a fixed point instead: each pass renders what
  it can, and a pass that resolves nothing new ends it. A cycle stalls and is
  reported like any other placeholder that never resolved.

- `resource list` and `tracker list` no longer drift once one row's value is
  long (#44).

  Both tables sized their columns from a hardcoded literal (`PROFILE` at 27,
  `AUDIENCE` at 9) rather than the row content actually being printed, so a
  resource with several profile traits, or a tracker whose audience string
  ran past the default width, pushed every column after it out of alignment
  for every row — not just its own. They now compute each column's width
  from every row plus the header, the way the branch-list table already
  does, so the table degrades to a wider column instead of to misalignment.

- A deposit-only tracker's bind line says `(deposit-only)` instead of
  `(0 files)` on `spawn`, `tracker pull`, and `undo` (#44).

  A tracker with no `paths` of its own exists only to receive `into_tracker`
  deposits, so it correctly has zero owned files to place in the workspace
  — but a real rev followed by `(0 files)` reads like a checkout that
  silently failed, right above the resources whose restore someone is
  usually there to debug. Naming the reason it's zero, rather than either
  printing the number or hiding it outright, is what actually removes the
  double-take: a bare omission still reads as a table with a value missing.

- A `hash` checkpoint's state ref can no longer reach a command as an
  argument ([#47](https://github.com/Spheroman/newgit/issues/47),
  [#48](https://github.com/Spheroman/newgit/issues/48)).

  `hash` records the content hash of `[identity] paths`. That answers one
  question — did the inputs move — and it never names a concrete thing to
  restore or tear down. But both places that resolve `{{state_ref}}` fell back
  to the recorded ref whatever it was, so a restore command got
  `restore-from hash:0fa284b468` and, worse, a cleanup hook got
  `delete-environment hash:0fa284b468` and *ran* it. The refusal that exists
  for exactly this case only asked whether a placeholder was still
  unresolved, so it caught the argument that was missing and not the one that
  was wrong.

  A hash ref now resolves to nothing at all. `{{state_ref}}` stays verbatim in
  a restore command, where an unresolved placeholder is already how a mistake
  is made visible, and the cleanup guard fires unchanged — one rule about what
  a state ref *is*, rather than a second check bolted beside the first.

  What the current definition *can* decide is refused when it loads.
  `[checkpoint]` and `[restore]` are two halves of one mechanism but were
  validated one section at a time, so every pairing loaded. Now a
  `{{state_ref}}` under a checkpoint that can never record one — `hash`, or
  `none`, or no `[checkpoint]` at all — fails at load, in a `[cleanup]`
  command as well as a `[restore]` one, since learning at teardown that the
  hook never ran is the worse half of the same mistake. The runtime guard
  still stands behind it, for the case reading the current file cannot
  predict: a record written under an older definition.

  These refusals are about the *placeholder*, not the mode. A restore command
  that never asks for a state ref is an ordinary rebuild whatever the
  checkpoint records, and still loads; so do pairings that are merely inert,
  like a `command` checkpoint under a `recompute` restore. The one mode
  pairing refused outright is `external` + `recompute`, which is not inert:
  `recompute` re-runs `prepare`, which for an external resource mints a
  second instance and orphans the one the handle names.

- A pre-undo checkpoint now says when its own contents are suspect, and no
  longer wastes a real capture command on a resource already known to be
  broken (#58).

  The message on a safety checkpoint — `state before undo to ckpt_001` —
  describes *when* it was taken, and a reader takes that as a description
  of what is in it. Those come apart precisely when the instance's last
  operation was an undo that did not finish: the workspace the next undo
  is about to capture is whatever that failed restore left behind, not a
  state anyone chose to be in. That checkpoint's message now says so:
  `state before undo to ckpt_001 (captured after an incomplete undo;
  contents may be partial)`.

  `newgit checkpoints`' `REASON` column already told a successful undo's
  safety checkpoint (`before-undo`) apart from a failed one's
  (`failed-undo`) and from a named one (`explicit`) — that distinction
  existed but was never covered by a test naming it directly, so a new
  test locks in the `(reason, undo_completed)` pair the column switches
  on.

  A safety checkpoint also no longer runs a resource's real checkpoint
  command — a `pg_dump`, say — against a resource whose own last restore
  already failed. That resource is known-broken, not merely unobserved,
  and spending the time to dump it produces the same rubble the message
  above now warns about, for a redo point nobody is likely to want. The
  skip is scoped to the automatic pre-undo path only: an explicit `newgit
  checkpoint` still runs the command, because a person asked for that one
  on purpose.

- `newgit reference` recommends a content-addressed package manager, in the
  one document that travels with the binary. Per-instance installs are the
  single place newgit multiplies a cost instead of absorbing it, and the
  choice that decides how much — pnpm or npm — is made once, early, by someone
  who has usually not read the README's section on it by then. The reference
  had a parenthetical `(a pnpm store)` in the ownership table and nothing
  else. It now says it plainly, with the per-tool costs and the two keys that
  wire a shared store up (`ownership = "user"`, `[identity] paths`).

- `newgit reference` documents `[[render]]`. The feature shipped in 0.2.0 but
  the reference did not learn about it, so the one copy of the definition
  format guaranteed to be wherever the binary is was the one place it was
  missing — exactly the gap `newgit reference` exists to close. The section
  covers `path`, `replace`, `find`/`with`/`count`, and the four rules; the
  template-variable scope table gains a `[[render]] with` row, and the note on
  unresolved placeholders now names `[[render]]` alongside `[cleanup]` as the
  other place they refuse rather than render verbatim.


### Documentation

- Changelog entries are written as one file per change in `changelog.d/`
  and folded into `CHANGELOG.md` at release time
  ([#36](https://github.com/Spheroman/newgit/issues/36)), and AGENTS.md
  records the two other conventions that keep parallel branches from
  queueing behind each other
  ([#37](https://github.com/Spheroman/newgit/issues/37)).

  Nothing about the entries themselves changes: they are still written when
  the change is made, still explain why rather than what, and are still the
  reason release notes are not generated from PR titles. Only where they are
  parked until release moved. Six branches appending prose to one
  `## [Unreleased]` section meant every branch conflicted with every branch
  that landed before it — four of seven rebases during the #23–#28 series
  hit `CHANGELOG.md`, and for three of them it was the only conflict, each
  resolved by the same mechanical act of keeping both entries in either
  order. Different filenames cannot conflict, so that queue is gone by
  construction rather than by being handled well.

- `[ports.<name>]` now says what order a resource's own ports are allocated
  in ([#59](https://github.com/Spheroman/newgit/issues/59)).

  `[ports]` is a map, not a sequence, and the reference only said how *one*
  port is chosen, not how a resource's several ports are ordered against
  each other. That matters as soon as two ports in one resource have ranges
  that can reach each other, which is the normal case for a tool whose
  defaults are consecutive. The allocation was already deterministic — a
  `BTreeMap`, iterated by key — and already matched display order, but
  nothing said so. The reference now states it, and a new test
  (`tests/port_alloc_order.rs`) pins name order against declaration order
  with two ports that share a start.

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

- **Committed definitions** (`config.toml`, `trackers/`, `resources/`,
  `exports/`) are
  the control plane. Changes here will be called out in this file.
- **Local state** (`branches/`, `checkpoints/`, `logs/`, `state/`, and
  tracker history in `local/trackers.git`) is derived or disposable. If a
  `0.x` release changes its format, the upgrade path is `newgit cleanup`,
  then re-spawn — workspaces are disposable by design.

Binding records and checkpoints are the exception worth caring about: they
are the only local state that is not reconstructible. A release that changes
their format will say so here explicitly.

**0.7.0 changes no formats you need to act on.** Two things cost a
little once. The install store now keys each tree on the trackers its
instance binds, so every existing entry misses: the first spawn at each new
key installs again, and `newgit cleanup` reclaims the old entries. The Linux
release archives are now `newgit-<arch>-unknown-linux-musl.tar.gz`; a
script that downloads the old `-gnu` names needs the new ones, and
`install.sh` picks them for you.

**0.6.0 changes no formats.** Nothing new is required: export targets
live in a new, optional `.newgit/exports/` (committed, like `trackers/`
and `resources/`), and a project without one behaves as on 0.5.0.
Workspaces pick up a `branches = true` target's remote at their next
checkpoint or push, so there is no need to re-spawn.

**0.5.0 does not read 0.4.0's local state; re-initialize.** Tracker content
moved from `.newgit/snapshots/` directories into a Git repository at
`.newgit/local/trackers.git`, and `newgit init` now copies the project's
history into the `root` tracker. A 0.4.0 project still loads under 0.5.0,
but its existing instances cannot checkpoint or undo (`tracker ... has no
captured content at rev ...`), and new ones cannot split commits into code
trackers, so `git push` and the remote do not work for it. `newgit init`
refuses to run again on a project that has `.newgit/`, so start it fresh:

1. Copy out any secret tracker files you want to keep (`.env.local` and
   the like) from each workspace. 0.4.0's captured content is not carried
   over.
2. Remove every instance, and drop their checkpoints:
   `newgit remove <name>` for each, then `newgit cleanup --purge-archived`.
3. Keep your definitions: copy `.newgit/trackers/` and `.newgit/resources/`
   somewhere, then `rm -rf .newgit`.
4. Run `newgit init`, then copy your tracker and resource definitions back
   into `.newgit/trackers/` and `.newgit/resources/`. `init` writes
   `root.toml`, `project.toml`, and `access.toml` itself, so leave those.
   Keep the new `config.toml`, and carry over any settings you had changed.
5. Re-spawn, put the secret files back, and `newgit tracker capture
   <tracker> <instance>`.

**0.4.0 changes no formats.** Nothing was added to the binding record or to
a checkpoint, so there is no migration and no reason to re-spawn. Two things
behave differently, both on purpose:

- **Port allocation got stricter, so a fresh `spawn` may pick different
  numbers than 0.3.0 would have.** The bind probe now checks `0.0.0.0` and
  `::` as well as loopback, which is what makes it see a Docker-published
  container port. A port that 0.3.0 would have handed out because it only
  looked at `127.0.0.1` is now correctly skipped. Ports already recorded in
  a binding record are untouched — allocation is permanent for the life of
  an instance — so an instance spawned by 0.3.0 may still be holding a
  number that was never really free. `newgit ports --check` is how you find
  out; the fix is to re-spawn that instance.
- **`spawn` exits 1 when a resource fails to bind**, where it previously
  exited 0 and said so only in the summary. A script or CI job that treated
  `newgit spawn` as infallible will now notice failures it was ignoring.
  The instance and its record are still created; the exit code reports the
  state, it does not roll the spawn back.

**0.3.0 adds fields to the binding record.** They all default, so a record
written by 0.2.0 loads unchanged and no migration is needed. Two things read
differently on an instance spawned before the upgrade, both of which correct
themselves and neither of which is a reason to re-spawn:

- `status` reports no base drift for it. The base it was spawned from was
  never recorded, and inferring one after the fact would be a guess — which
  is the failure this feature exists to remove, not reproduce.
- Every resource with a `command` or `recompute` restore reads `restore
  never exercised` on its next checkpoint, even if its restore has worked
  for you before. The claim is "newgit has watched this complete", and on a
  fresh field it has not. One successful `undo`, or one `newgit checkpoint
  --verify`, clears it for good.

[Unreleased]: https://github.com/Spheroman/newgit/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/Spheroman/newgit/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/Spheroman/newgit/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/Spheroman/newgit/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Spheroman/newgit/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Spheroman/newgit/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Spheroman/newgit/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/Spheroman/newgit/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Spheroman/newgit/releases/tag/v0.1.0
