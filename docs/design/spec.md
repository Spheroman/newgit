# newgit design

*Why newgit is shaped the way it is: the model, the decisions behind it, and
what they cost.*

This document describes newgit as it exists on `main`. It explains the
design, not the commands. For those, and for the rest of the documentation:

- [The guide](../guide.md) covers how to use newgit, command by command.
- [`definitions.md`](../../crates/newgit/reference/definitions.md), which
  `newgit reference` prints, lists every key in a tracker, resource, or
  export definition, with its default and the template variables each hook
  sees. Run `newgit resource templates --show <name>` to read a starter
  template in full.
- [`trackers.md`](trackers.md) covers the tracker model in depth: root,
  project, access, and code trackers, how a workspace history is composed
  from them and split back, roles, binding, and merge.
- [`remote.md`](remote.md) covers the newgit.dev remote.
- [`vision.md`](vision.md) is the original long-range architecture. Most of
  it is not built.

---

## What newgit is for

newgit answers one question:

> Can parallel agent branches feel effortless if a developer defines, once,
> everything a branch needs to run, and newgit makes it move together?

Git tracks the code. Everything else a branch needs to run lives outside
Git: the `.env.local`, the installed dependencies, the dev server, the
database, the ports. With one branch checked out at a time, that is fine.
With five agents working in parallel, they all share it, and it breaks.

newgit turns a branch into an **instance**: a source branch, a workspace, the
files Git should not hold, and the things that have to be running. All of it
is spawned, checkpointed, undone, shared, and torn down as one unit.

The first user is a developer running several coding agents against one
project on one machine. The design assumes:

- Agents are trusted enough to run as the local user.
- The project already uses Git, and keeps using it. GitHub stays where it
  is.
- The pain is coordination, not an adversary.

So newgit makes no security claims. Instances are not isolated from each
other, a tracker's audience is not enforced against local code, and ports
are allocated per instance, not sandboxed. The newgit remote does enforce
audience, on the server, because there it can (see *The remote*).

newgit is also not a VCS, a filesystem, or a build system. Source history is
Git's. Workspaces are ordinary directories. Installs call the package
manager the project already uses; a Nix project's resource calls Nix. newgit
binds those substrates together and replaces none of them.

---

## The model

An instance is:

```text
source branch
+ the trackers it binds, each at a revision
+ the resources those trackers bring, each with its ports, exports, status
+ a workspace directory
+ its checkpoints
```

That record lives in `.newgit/branches/<instance>.toml` and is the central
object. The workspace is disposable; the record is not.

### Two primitives

Everything an instance carries is one of two things.

> A **tracker** is a named, versioned set of paths: file content with its
> own history and its own audience.
>
> A **resource** is a lifecycle unit: it re-establishes per-instance state
> that cannot be carried as content.

The line between them:

> **Trackers hold state that can travel**: to another machine, or back from
> a checkpoint. **Resources re-establish the state that cannot**, because it
> is alive, lives in another system, or is only valid where it was built.

Resources exist for exactly three reasons:

- **Liveness.** A running process cannot be copied, only started. A port
  cannot be snapshotted, only allocated. A daemon's state can only be
  captured consistently through the daemon.
- **Externality.** The state lives in another system. Locally there is at
  most a handle, and an API call is the only interface.
- **Path-dependence.** Installed artifacts (`node_modules`, a venv, a native
  build) are tied to a machine and a path. The truth is the identity, the
  lockfile, and the artifact is derived from it. Instances whose identity
  agrees can share the derived tree through the install store (see
  *Installs*); instances whose identity differs never do.

If something is just files that copy correctly, it is a tracker.

The two meet at one seam: **a resource's checkpoint can deposit its output
into a tracker.** A Postgres resource's `[checkpoint]` runs `pg_dump`, and
the dump becomes versioned content in a `db-snapshots` tracker, restorable
and syncable like any other.

### No hardcoded worldview

Real projects have odd per-branch things: a Redis namespace, a Stripe
webhook tunnel, a seeded search index, a cloud preview, a mock auth tenant.
newgit should not need a subsystem for each one. The core has no env
system, no database system, no package manager support. It has tracker
definitions and resource definitions, and an agent reading the config can
tell from the noun alone whether a thing travels (tracker) or is rebuilt on
each machine (resource).

The test of the design:

> Can a user model a tracker or resource the newgit authors did not
> anticipate?

The starter templates (`process`, `pnpm`, `install`, `command-snapshot`,
`command-snapshot-migrations`, `supabase`, `external`) are definitions like
any other, written to `.newgit/resources/` and owned by the project from
then on. Each states the host tools it assumes, since "ready to adapt" and
"ready to adapt if your database runs on the host" are different claims. A
template may bring the companions it needs, such as the `pnpm-store`
resource it depends on or the `db-snapshots` tracker its checkpoint
deposits into. They are created only when absent, so `resource add` never
overwrites a definition.

### Committed definitions, local state

`.newgit/` splits into a control plane and a data plane:

```text
.newgit/
  config.toml            [project], and [remote] once linked   committed
  trackers/              tracker definitions                    committed
  resources/             resource definitions                   committed
  exports/               export targets                         committed
  scripts/               scripts resource commands call         committed
  branches/              binding records, one per instance      local
  checkpoints/           checkpoint records                     local
  installs/              the install store                      local
  logs/  state/          action logs, PID files                 local
  local/trackers.git     every tracker's history (bare repo)    local
  local/publish/         the push outbox                        local
  local/locks/           one lock per instance                  local
```

Definitions are committed, so teammates and CI see the same orchestration.
Concrete state stays local: bindings, captured content, snapshots,
allocated ports, logs, external handles. Secret tracker content stays out of
Git by construction, and travels only through a newgit remote.

Definitions are one per file, named by the filename. That gives clean diffs,
matches what `tracker create` and `resource add` produce, and avoids TOML's
array-of-tables footgun. An unknown key is an error naming the file and the
key, never a line quietly skipped: a misspelled `ownership` would otherwise
decide what `remove` may delete.

Workspaces live outside the project, under
`~/.newgit/workspaces/<project>-<hash>/<instance>/`. The hash is of the
project's path, so two projects named `api` do not collide. The committed
config never names a workspace root, so it never bakes in one user's paths.

---

## Rules that recur

A handful of rules decide most of the questions below. They are listed here
so the later sections can point at them.

**Interpose, don't emulate.** Run the real command and add newgit's side
effects. Never make a command secretly do something else. A workspace is a
real Git repository, `git push` is Git's own push, and `.git` passes `git
fsck`. A 95%-correct illusion is worse than none: an agent that notices it
will debug it, decide the repo is corrupt, and start "fixing" it. Everything
the environment claims must be checkable with the tools in that environment.

**An incomplete operation must say so.** A spawn, undo, or restore that did
not fully work leads with that verdict and exits non-zero. It does not say
"Restored" on the first line and "FAILED" on the fourth. A caller that only
checks the exit code must be able to tell "ready" from "spawned but broken".

**Report precisely, not generically.** A warning that fires when nothing is
wrong trains people to ignore the one that matters. newgit warns when it
knows something specific (this file has changes a re-render will discard;
this capture was declared and never printed) and stays quiet otherwise.

**Destructive writes fail closed.** Rendering leaves an unknown `{{...}}` in
a command verbatim, because `{{.Field}}` may belong to another tool and the
reader can tell. But a `[cleanup]` command, a `[[render]]`, an `[exports]`
value, and a `[restore]` command's `{{state_ref}}` each write something that
outlives the command, so an unresolved variable there refuses the run. A
destructive command with a literal placeholder is not a no-op; it is a wrong
argument.

**Never break an undo.** Nothing newgit does on its own initiative deletes
what a checkpoint points at. Discarding history happens only when someone
asks for it by name.

**Teardown never depends on the graph.** A broken definition is reported,
not raised at load. Commands that act on the graph (`spawn`, `run`,
`action`, `checkpoint`, `undo`) refuse with the offending name. Commands
that build or inspect it (`tracker create`, `resource add`, `status`) warn
and run, since they are how a broken graph gets fixed. `remove` and
`cleanup` always work.

**Name-inference.** Lifecycle commands are top-level verbs; definition
management uses nouns (`tracker create`, `resource add`). An instance
argument is optional inside a workspace, because newgit chose the workspace
path and can always map the current directory to its instance. Agents
standing in a workspace type `newgit status`, not `newgit status
feature-a`, and both work.

---

## Trackers

A tracker is a definition in `.newgit/trackers/<name>.toml`: the paths it
owns, who may read it, which resources it brings, and which role it fills.
Its history is a Git repository: locally, refs under
`refs/trackers/<name>/` in `.newgit/local/trackers.git`, with a `main` (what
new instances get) and a branch per instance that changed it. On the
remote, it is that tracker's own repository.

The full model is in [`trackers.md`](trackers.md). The short version:

- **Every path has exactly one owner**, the most specific claim. `root`
  owns `.`, everything nothing else claims. `project` owns `.newgit`.
  `access` owns `.newgit/trackers/`, which decides who may read what, and by
  default only the project's owner may change it (`maintainers = "owner"`).
  `newgit init` creates all three and copies the project's Git history into
  `root`.
- **Code trackers** (`secret = false`) are committed content with their own
  history. `newgit tracker create electron --code --path apps/electron`
  carves those paths, history included, out of whichever tracker owned
  them. You commit to them with Git. When work leaves the machine, newgit
  splits each commit into per-tracker commits, and composing them back
  gives the same commit, with the same id.
- **Secret trackers** (the default) are files that never belong in Git: an
  `.env.local`, an agent's notes, a SQLite file. They are captured by
  newgit, never committed.
- **Binding** decides what an instance works on. Trackers sharing a `role`
  are alternatives (`local-db`, `staging-db`), and an instance binds one per
  role: `spawn --use`/`--drop` first, then your own default in
  `[projects.<project>.roles]` in `~/.newgit/config.toml`, then `priority`.
  A negative priority is never bound unless asked. `newgit tracker use` and
  `drop` change a live instance's binding. A tracker brings resources, and
  they run only while it is bound.

### Secret trackers

Capture snapshots a secret tracker's paths into a new revision on the
instance's branch. `newgit tracker merge` promotes that revision to the
tracker's `main`; `tracker pull` brings `main` into an instance; `tracker
checkout --rev` restores an exact revision. Checkout clears the owned paths
first, so it reproduces the captured state exactly, and captures the
current content before overwriting it, so it is always undoable.

A tracker starts empty, so adopting newgit would mean spawning an instance
without its env file, copying the file in, capturing, and merging. `tracker
capture <tracker> --from-store` reads the owned paths from the project's own
checkout and sets `main` directly instead, so the next spawn comes up with
them. A tracker that has never captured anything projects nothing, the same
as Git: tracking a path does not invent content for it.

Because a secret tracker is pure content, capture and restore need no
modes. If something needs a command to capture or restore, it is a resource.

Secret content is kept in its own repository, not the project's. The
project repository is what a local `git clone` hardlinks into every
workspace and what a push publishes from; the tracker repository lives
under `local/`, which is gitignored.

### Keeping secret paths out of Git

A secret tracker's files sit inside a Git clone, so an agent could `git add`
them. The rule:

> **A path owned by a secret tracker must be ignored by Git.**

`tracker track` appends the path to `.gitignore`, loudly. That is not
enough on its own: the edit is uncommitted in the project checkout, so a
workspace cloned from it does not inherit the rule, and the files would sit
there as untracked content that `git add -A` sweeps up. So `spawn` also
writes every tracker-owned path, and every path an install produces, into
the workspace's `.git/info/exclude` before any content lands. `info/exclude`
rather than the workspace's `.gitignore`, because that is a committed file
and newgit does not rewrite the user's files.

That still does not stop `git add -f`. The backstop is at the point where
work leaves the machine: a push or a checkpoint sync that would carry a
commit adding or changing a secret tracker's file is refused, naming the
commit and the file. History from before `newgit init` is exempt, so an old
committed `.env` does not block a first push; removing it is a history
rewrite, which is not newgit's to do silently.

### Audience and storage

Every tracker has an `audience` (`public`, `project-devs`, `user`, or
`user:<login>`) and a `storage` (`local` or `remote`).

Locally, audience decides what `newgit export` ships and nothing else. On
the newgit remote it is enforced: the remote mints tokens per tracker, so a
reader gets only the trackers they may read, and a branch composed for them
holds only those code trackers.

`storage = "remote"` makes a secret tracker sync. A checkpoint pushes its
revisions to the remote; `spawn --from-checkpoint` fetches them. A tracker
with `audience = "user"` is per user: each person's content is their own
repository on the remote, so spawning a teammate's checkpoint gives you
your own `.env.local`, never theirs. `tracker create --storage remote`
is refused until the project has a remote, rather than quietly behaving as
local.

`merge_with_source` is written to every definition but nothing reads it
yet. `newgit merge` merges code trackers, and secret trackers move only
through `tracker merge`. [`trackers.md`](trackers.md) plans to replace it
with `merge = "with-change" | "explicit"`.

---

## The workspace

The materializer contract:

> A workspace presents a full, real, verifiable Git repository at the
> binding record's revision, plus every bound tracker's paths. Nothing else
> in newgit may care how that is produced.

### Independent clones, not worktrees

newgit does not use `git worktree`. Worktree friction is one of the pains
that motivated the project, and it is structural: worktrees share one
gitdir (refs, config, hooks, gc), each branch can be checked out in only
one of them, their lifecycle is registered in the main repository (delete
the directory and you owe a `git worktree prune`), and their
`.git`-as-a-file breaks real tools.

Instead, `spawn` makes the workspace a complete, standalone clone of the
project repository:

- A local `git clone` hardlinks objects, so it is fast. Hardlinked objects
  are safe, since gc never rewrites an object file in place. `--shared` and
  `--reference` are not safe, because the other repository's gc can delete
  what they borrow, and newgit never uses them.
- The agent gets a fully real repository. `.git` is a directory, and `git
  fsck`, `git log`, and everything else check out.
- Deleting a workspace is `rm -rf`. Nothing to register, prune, or lock.
- Nothing is shared between workspaces, so two instances of the same base
  branch is a non-event.

On macOS the clone is not made by `git clone`, which spends almost all of
its time checking files out (2.3s of 2.5s on T3 Code's 28.6k files).
newgit keeps one clean checkout of the project repository beside the
workspaces, `<workspace root>/.source-template`, with no local branches.
`spawn` fetches it from the project repository, checks out the instance's
revision, and copies the whole tree with one `clonefile(2)`. Then the copy
gets its own branch, with the upstream `git clone --branch` would set, and
`core.checkStat=minimal` plus `core.trustctime=false`. Without those two
settings Git would re-hash every file on the first `git status`, because
every inode and ctime is new. With them it compares a file's size and
whole-second mtime only, both of which the copy keeps. The result is the
same repository a clone produces, in 0.67s instead of 2.7–3.1s. The first
spawn builds the template and takes longer once. If anything in this path
fails, `spawn` falls back to `git clone`. That includes a workspace root on
another volume, which `clonefile` cannot reach.

What those two settings give up: an edit Git cannot see is one that keeps
the file's size and the second of its mtime. Ordinary edits cannot do that.
They are later than anything Git recorded, and Git re-hashes any entry
written in the same second as its index. A copy that keeps its source's
mtime (`cp -p`, `rsync -a`, `tar x`) carries that file's own second, not the
one Git recorded. What remains is setting a file's mtime to the recorded
second on purpose (`touch -r`) while keeping its size, which Git's default
settings would catch through the ctime. Even then only the workspace's own
`git status` misses it. A checkpoint builds its tree in an index of its own
with no stat data, so it hashes every file and captures the edit.

If the template path fails, `spawn` says why in its output
(`template_warning` in `--json`), since a template that fails every time
makes every spawn pay for rebuilding it on top of the clone.

A dropped code tracker's files are left out of the working tree with Git's
sparse checkout. The history still has them, so every instance of a project
has the same commit ids.

Sync is newgit's job, and it runs one way: a checkpoint **fetches** from the
workspace into the project repository. Fetching avoids the
push-to-checked-out-branch problem and keeps workspaces passive. The project
repository only learns about a workspace's commits when a checkpoint
records them, so an agent can commit, rebase, and make a mess in its clone
without touching anything else.

### `git push` publishes

A workspace is cloned from the project repository, so its `origin` would
start as a local path. Left that way, `git push` would land in the project
repository, move a branch behind checkpoint's back, reach nobody, and
report success; `git pull` would read the wrong place; and `gh` cannot map
a path to a repository at all. So `spawn` routes `origin`:

- **Fetching reads the real remote**: the project's own `origin`, or the
  project's newgit endpoint when it has one. `git pull` and `gh` see the
  real repository.
- **Pushing goes through newgit's outbox**, a bare repository in
  `.newgit/local/publish/outbox.git`. Before Git advertises anything to the
  pusher, newgit brings the outbox up to date with the real remote, so the
  pushing Git judges fast-forward and `--force-with-lease` against what that
  remote holds now. Then the outbox's `pre-receive` hook:
  1. **refuses secrets**, as above;
  2. **checkpoints the instance**, because `undo` cannot take back a push,
     so the push is the one moment a checkpoint has to exist (it is listed
     with reason `push`);
  3. **forwards the push** and succeeds exactly when the real remote
     accepted it. A store with no `origin` refuses every push: nothing was
     published, so nothing may look published.

Why an outbox and not the project repository: checkpoint and undo move the
project repository's branch to the workspace's head, so after any
checkpoint a `--force-with-lease` push judged against it would fail as
stale. The outbox holds exactly what the real remote holds.

Why not push to the real remote directly: then nothing could checkpoint
first. A workspace's own hooks are skipped by `--no-verify` and replaced by
husky's `core.hooksPath`.

The route is Git's own receiving side, not a shim. The workspace's
`remote.origin.receivepack` runs `newgit push-receive-pack`, which hands
over to `git receive-pack` with the outbox's hook selected through `-c
core.hooksPath`, so nothing is written to the developer's hooks and a
global `core.hooksPath` cannot switch it off. The route is per-clone
config, and every checkpoint reinstalls it.

On a newgit remote, `origin` is the newgit endpoint, which `gh` cannot read.
An export target with `branches = true` adds itself as a remote in each
workspace and as the one `gh` resolves, so `git push` then `gh pr create`
opens a real pull request there (see *Export*).

### The materializer is replaceable

Clone is the first materializer, not the last. Each later step keeps the
contract fixed:

1. **Now:** an eager local clone (on macOS, a `clonefile` of a template
   checkout).
2. **Later:** a Git partial clone with the project repository as the
   promisor remote, so objects arrive on demand through Git's own lazy
   mechanism. Big repositories spawn fast without newgit touching a
   filesystem.
3. **Eventually:** a projected filesystem (FUSE) that replaces the clone
   and answers every read exactly as a clone would. A projection that cuts
   a corner, such as a `.git` that fails `fsck`, is emulation, and that is
   the line it must not cross.

So nothing else may assume a workspace is a clone. Checkpoint reads source
through Git's interface (a fetch), never by reaching into `.git`, and
works the same against either.

---

## Resources

A resource is a definition in `.newgit/resources/<name>.toml`: its
ownership, dependencies, identity, ports, exports, renders, actions, and its
checkpoint, restore, and cleanup hooks. Not every resource uses every part.
Every key is in [`definitions.md`](../../crates/newgit/reference/definitions.md).

### Ownership

`ownership` says who owns the concrete thing and what teardown may touch.
It is operational, not a security label.

| ownership | means | per-instance teardown |
| --- | --- | --- |
| `branch` | one per instance (a database named after the branch) | runs its hook |
| `workspace` | lives in the workspace (`node_modules`) | runs its hook |
| `project` | shared by this project's instances | never |
| `user` | shared beyond this project (a pnpm store) | never |
| `external` | another system owns it; newgit holds a handle | exactly the `[cleanup] command`, nothing else |

`project` and `user` are where destructive defaults would be easiest to
write by accident. A pnpm store is shared with every project on the
machine, so newgit never deletes or rewrites one, at `remove` or at
`cleanup`.

### Actions

Actions are named commands: `newgit action <resource>.<action> [instance]`.
The resource comes first and the instance last, so an agent in its
workspace types `newgit action app.start`. Most names are free. A few mean
something to newgit:

- **`prepare`** makes the resource ready. `spawn` runs it, in dependency
  order.
- **`start` and `stop`** are a convention made real by `long_running =
  true`: the process starts detached in its own process group, its output
  goes to `.newgit/logs/`, its PID to `.newgit/state/`, and `stop` sends the
  configured signal (TERM by default) to the group. No daemon, no restart
  policy. `autostart = true` has `spawn` start it; the `process` template
  sets it.
- **`suspend` and `resume`** take down and bring back something newgit does
  not supervise, such as a Supabase stack or a Compose project. `newgit
  stop` and `start` run them (see *Stop and start*). Each needs a command,
  and neither is accepted without the other.

A hook is not an action: `[checkpoint]`, `[restore]`, and `[cleanup]` each
run at one moment and against state only that moment has, so none can be
invoked by name.

`--dry-run` prints the rendered command, the directory it runs in, any
captures, and whether it would be supervised, and runs nothing. It shares
its resolution with the real run, so it refuses for exactly the same
reasons. A `{{...}}` left in the rendered command is called out, since only
the reader can tell a typo from another tool's syntax.

**Captures.** Some handles are minted by another system: newgit cannot
compute a preview id, only ask for one and remember the answer. An action's
`captures` reads declared names from its stdout (a flat JSON object, or
`KEY=VALUE` lines) into the binding's exports, where `run`, later hooks, and
`cleanup` can use them. When `captures` is set, stdout belongs to newgit,
so the action's output goes to its log. A declared name the command never
printed stays absent rather than failing, since a resource may publish a
handle only on some runs, but newgit warns, naming the capture and the log.
Otherwise the result is an action that exits 0, reports `ok`, and publishes
an empty handle nobody notices until an API call returns 401.

### Where a resource's script lives

A resource definition is read from the project checkout, not the
workspace. If the scripts it called were read from the workspace, the two
halves of one definition would follow different rules: the TOML editable in
place, the script needing a commit before any spawn could see it.

So `{{scripts}}` resolves to `.newgit/scripts/` in the project checkout.
Edit either half and the next `newgit action` uses it, with nothing to
commit first. Scripts still run with the workspace as their working
directory. The directory is control plane and belongs in Git. A script the
*application* runs stays in the project tree and is read from the
workspace.

### Dependencies

`depends_on` is a lifecycle claim. Needing another resource's value is a
separate, weaker thing that is not declared at all: a
`{{exports.<name>}}` in an `[exports]` value or a `[[render]]` already
names what it needs, so newgit reads the edge out of the template. That
gives two orders:

- **Bind order**, `depends_on` plus those data edges. Binding trackers,
  preparing, rendering, assembling the environment, and restoring all walk
  it, because a value must exist before a template that reads it renders.
- **Lifecycle order**, `depends_on` alone. Checkpoint, stop, and cleanup
  walk it in reverse, so needing one string from a resource claims nothing
  about the order the two are torn down in.

If a dependency fails to prepare, the instance still spawns. Its dependents
are `blocked`: their `prepare` does not run, and their command actions are
refused until the dependency is repaired, while signal-only actions such as
`stop` stay available. Nothing is rolled back, so the failure stays there
to inspect and the action can be re-run.

A name in the environment belongs to exactly one declaration. Two
resources exporting the same name is a graph error, not a last-one-wins
surprise found as a wrong variable in a subprocess.

### Ports

**An instance's port never changes once allocated.** At bind time newgit
takes the first port scanning up from the declared `start` that no other
instance's record claims and that is free right now. "Free" means it binds
on all of 127.0.0.1, 0.0.0.0, ::1, and ::, with `SO_REUSEADDR` off, so a
Docker container published on a wildcard address cannot hide behind a
loopback-only check (#93).

The binding records are the only ledger. Removing an instance frees its
ports, with nothing separate to drift. A resource's ports are written into
the instance's record before its `prepare` runs, under a short lock held
only while they are chosen, so two spawns installing side by side are never
handed the same port.

A record can only be as complete as the bind probe, and a container a
resource starts can publish a port the record never learned about (#92).
`newgit ports --check` catches that from the other side: it reads the real
socket table (`lsof`), keeps listening ports inside some resource's range,
and reports any that no record claims. It names an instance only when the
instance's name or workspace really appears in the listener's command
line. On macOS, Docker Desktop fronts container ports with its own proxy,
so most Docker conflicts come back unattributed. An unattributed conflict is
still the useful finding; a confidently wrong instance name would not be.
If `lsof` is missing or fails, `--check` says it could not check rather
than reporting every port free.

### Exports and the command environment

Every command newgit runs for an instance, whether `newgit run`, an action,
or a `[checkpoint]`, `[restore]`, or `[cleanup]` hook, gets one
environment: each resource's `[exports]` in dependency order, the ports'
`env` variables, captured values, and `NEWGIT_BRANCH` and
`NEWGIT_WORKSPACE`. It is one assembly, not one per hook, because the hook
it matters most for is the one you least want to guess at: a `[restore]`
that resets a database picks *which* database from that environment, and
one missing a variable falls back to the developer's shared default.

`newgit env` prints that assembly, one `export NAME=value  # declaration`
line per variable. It is the same code path the hooks use, not a
reconstruction that could drift from it, and the single-owner rule is what
makes each label honest. It is one command rather than a `--resource`
filter, because the environment does not vary by resource. Values print
verbatim, credentials included. Redacting them would be simulated security:
anything that can run `newgit env` can run a hook that echoes the value.

### Render

Ports reach a command as `{{ports.x}}` on its command line or as a variable
in its environment. Most tools take neither. Supabase reads its ports from
`supabase/config.toml`, Expo from `.env`, Compose from `compose.yaml`,
Rails from `database.yml`: a file the project commits. Without a first-class
answer, every project writes the same config rewriter inside its `prepare`,
and the interesting part of the resource stops being `supabase start`.

`[[render]]` is that answer, declared on the resource that owns the
values:

```toml
[[render]]
path = "supabase/config.toml"
replace = [
  { find = 'project_id = "faretable"', with = 'project_id = "faretable-{{branch.slug}}"' },
  { find = "port = 54321",             with = "port = {{ports.api}}" },
]
```

The exact rules are in the reference's `[[render]]` section. The decisions
behind them:

**The committed file is the template.** There is no template file. `port =
54321` is not a placeholder; it is Supabase's working default, so someone
who clones without newgit still gets a working stack. A template would mean
a second copy of the config under `.newgit/`, kept in sync with upstream
forever, and a project whose real config has been hollowed out into
placeholders. Substituting into the committed file has neither problem.

**`find` is literal, never a regex.** A pattern language brings back the
"did it match what I meant" doubt the next rule removes, and it would make
the inverse undefined.

**A `find` must match exactly once** (or a declared `count`), or the bind
fails, naming the file and the string. One rule does four jobs:

- Ambiguity is impossible. A bare `54321` that also appears in a comment is
  caught before anything runs.
- Drift is loud. When upstream changes its default, the `find` stops
  matching and the bind fails by name. The check *is* the drift detector,
  and it runs on every bind for free.
- Disambiguation needs no parser. When two sections share a value, `find`
  goes multi-line (`[api]\nport = 54321`). newgit never learns TOML, which is
  what lets the same mechanism serve `.env`, YAML, and `vite.config.ts`.
- The inverse stays well-defined (below).

`count` is a number, not `all`, so it keeps failing when two occurrences
become three.

**Replacements are simultaneous.** Every `find` is located in the committed
content, then all replacements apply as one batch, and a replacement's
output is never a match target. So the order of the `replace` array is
cosmetic, and a reader does not have to simulate the list to know what it
does. Two rules claiming overlapping text have no batch answer and are
refused.

**Render reads committed content, never the working file.** For a path
Git tracks, that is the blob at `HEAD`; for a tracker-owned path, the bound
tracker revision. Otherwise the second render would look for `port =
54321`, find `port = 54400`, and fail. With this rule, render is pure and
idempotent: `undo` and `tracker pull` re-render from the record, and values
can never compound. Render runs at bind, after ports are allocated and
dependencies are bound, just before `prepare`, and again before a
`recompute` restore. A template sees exactly what a command in the instance
would see in its environment, so an Expo `.env` can name the Supabase
resource's URL.

**Instance values must not leak.** A render writes one instance's values
into a path the project owns. Four places they must not reach:

- **`git status` and `git commit`.** Rendered files Git tracks are marked
  `--skip-worktree` in the workspace, so `git add -A` cannot commit this
  instance's port. It is the same job `info/exclude` does for tracker paths.
- **`newgit export`**, which copies tracked files as they stand on disk.
  It takes rendered paths from `HEAD` instead.
- **Tracker captures.** A tracker-owned file cannot be skipped: the tracker
  is how the `.env` reaches other instances, and the user may have added a
  key to it. So every capture, including a checkpoint's and the one `tracker
  pull` takes first, **reverses the substitution** (`54400` back to
  `54321`). Real edits survive; the instance's values never enter the
  tracker. This works only because the substitution is literal, and it is
  the strongest argument for it: a whole-file template cannot run
  backwards. The rendered value must therefore also be unique in the file,
  checked at bind so the failure comes then and not at capture.
- **A checkpoint's uncommitted-work commit.** It is built in a throwaway
  index, which does not carry the real index's skip-worktree bits, so newgit
  sets them there too.

**What it costs, said precisely.** On a file Git tracks, skip-worktree
means real edits to it in the workspace are invisible to newgit and die
with the workspace. That is right for values newgit generates and wrong for
an `[auth]` block someone adds by hand. A warning at bind would fire when
nothing is wrong and stay silent when the work is lost. Since a render is a
pure function of committed content and the record, the expected bytes can
be recomputed any time, so newgit compares at the two moments that matter:
at checkpoint, because a checkpoint cannot carry that edit, and before a
re-render, because that is when it is overwritten. The report names the
file and how many lines differ, and says nothing when the file is what the
render produced. To change a rendered file for real, change it in the
project checkout and commit it. Tracker-owned targets do not pay this cost.
Replacing skip-worktree with a clean/smudge filter would remove it (#64).

**Rendering into a built tree.** A render may target a file inside its own
resource's `[identity] produces`. That is how the install store shares a
tree that records its own workspace path as data: storing reverses the
render, and filling renders it forward for the new instance. The `pnpm`
template uses it for `node_modules/.pnpm-workspace-state-v1.json`.

**`render --check` is the adoption dry run.** Reading committed content has
one cost: while you are writing a `find`, the string is not committed yet,
so learning that it is wrong would take a commit and a spawn.
`newgit render --check` matches every `find` against the working tree
instead, with no instance and nothing substituted. Outside adoption, it is a
drift check a project's CI can run. A `spawn --render-from-worktree` was
considered and not built: it costs a full spawn to learn one string is
wrong, and it adds a second rule for what render reads.

### Installs

An install is a resource, not a tracker. The installed tree is
path-dependent and derived; its truth is the lockfile. Two instances with
different lockfiles must never share a tree, or one branch's install
silently rewrites the other's dependencies.

But two instances at the *same* lockfile are not two derivations. They are
one tree, built twice. A resource that declares `[identity] produces` joins
the **install store** (#50): the first instance at a key installs and
publishes its tree to `.newgit/installs/<resource>/<key>/`, and every later
instance at that key is filled from it and does not run `prepare` at all.
On T3 Code's own monorepo, that takes a second spawn from about 50 seconds
to about 5.

The key is the inputs, never the tree:

- **`[identity] paths`**, what the tree is derived from (lockfile and
  manifest).
- **`key_command`'s output**, what the lockfile cannot see. A lockfile says
  what was asked for, and install scripts compile against a platform and a
  toolchain it never names (`node -v && uname -sm`).
- **The trackers the instance binds.** A shared lockfile lists every
  workspace member, bound or not, and the install builds the bound ones.
  So instances that bind differently never share a tree, and when a live
  instance's binding changes (`tracker use`, `drop`, or an undo to another
  binding), its installs are rebuilt for the new key, from the store where
  possible (#206).
- **The definition file.** The command that builds a tree is as much an
  input as the lockfile. Any edit moves the key, which costs one reinstall;
  being wrong the other way would hand out a tree the old command built.

What keeps sharing safe:

- **Copies fork on write.** Fills use `clonefile` on APFS and `--reflink`
  on btrfs/XFS, never hardlinks. `node_modules` is not read-only after
  install: native builds write into it, and under hardlinks one instance's
  build would rewrite every other instance's tree. Where the filesystem
  cannot clone, newgit takes a full copy and says so. Same behaviour, worse
  performance.
- **A tree that names its own workspace is not shared**, unless a render
  covers the file. Copy-on-write cannot fix a path already baked into the
  content, so admission scans the tree and declines it, naming the file.
  That resource keeps installing per instance. Nothing fails.
- **`prepare` is the only thing an entry stands in for.** A definition
  where something else builds the tree (a `long_running` or command-less
  `prepare`, `captures` it would stop publishing, a `recompute` restore
  naming another action) is refused rather than left never to fill.
  `newgit action <resource>.prepare` always runs the command, since someone
  asked for it by name, and publishes the result.
- **Entries are a cache.** `newgit cleanup` drops entries no live instance
  keys to, and anything a killed spawn left half-copied. The cost of being
  wrong is an install. `undo --force-recompute` means "do not trust the
  identity to describe the tree", so it bypasses the store and drops the
  entry too.

Produced paths go into each workspace's `info/exclude`, so a checkpoint's
`git add -A` never sweeps an install into source history.

The package manager still sets what a miss costs. pnpm, uv, and bun
hardlink from a shared store; npm and pip into a venv expand a full copy
per instance. The reference has the table, and the `pnpm` template ships a
`user`-owned `pnpm-store` resource so newgit never touches the shared
store.

---

## An instance's life

### Spawn

`newgit spawn <name>` creates or adopts the source branch, clones the
workspace, chooses the binding, writes the excludes and the push route,
projects the bound trackers' content, then binds resources in order:
allocate ports, write the record, render, export, and run `prepare`, or
fill from the install store. Then it starts each long-running action marked
`autostart`, dependencies first, so a dev server is up when the agent
arrives. With a remote, it registers the instance there.

Autostart comes after every `prepare`, not interleaved with them, because
`prepare` is a command that finishes and a dependent's `prepare` may need
its dependency's files but never its process. A start that fails is
reported per resource, and a dependent of a resource that did not start is
not started against it. Starting is not waiting: the dependent starts once
its dependency's process exists, not once that process answers (#54).

`spawn --from-checkpoint <instance>/<checkpoint>` brings up someone else's
checkpoint, from the remote: their source and uncommitted work, the
trackers you may read (your own copy of per-user ones), and resources
restored from the checkpoint, such as a database loaded from its dump. It
is built from parts that already exist: fetch, spawn at the checkpoint's
head, then undo to the checkpoint. Its `autostart` actions start after the
undo, since the restore stops and restarts processes to match the
checkpoint.

#### An incomplete spawn must say so

A failed `prepare` does not abort the spawn. A half-bound instance with a
record, a workspace, and logs is more useful than none. But then the
summary can contain a `FAILED` line for a resource that never came up, and
a caller that checks only the exit code cannot tell "ready" from "spawned
but broken". So `spawn` exits 1 whenever any resource did not bind, and its
last line says so:

```text
spawned with failures: 1 of 4 resources did not bind.
  `perf-a` exists but is not ready. See the FAILED/BLOCKED lines above.
```

A blocked resource counts. Its `prepare` never ran, so it is no more usable
than one whose `prepare` failed; only where the failure started differs.
Every surface (`spawn`, `spawn --json`, `status`, `action`) derives
"blocked" the same way, from the graph at read time, so a script and a
person see one instance (#209). A resource that failed itself is `failed`
even with a dependency down too, because repairing the dependency would not
fix it. An `autostart` action that did not start counts too: an instance
whose dev server is down is not up. The instance is not rolled back.
`--from-checkpoint` judges by the instance the restore leaves (#208).

### Run and env

`newgit run [instance] -- <command>` runs a command in the workspace with
the instance's environment, logging its output to `.newgit/logs/` as well
as the terminal. `newgit env` prints that environment (see *Exports and the
command environment*).

### Stop and start

`newgit stop` takes an instance's resources down without removing it, for
an instance someone is not using right now: a settled T3 Code thread, say
(#157). It stops every running long-running action, dependents first, runs
each `suspend` once that resource's dependents are down, and records what
it took down in `.newgit/state/<slug>/stopped.toml`. `newgit start`
brings back exactly that, dependencies first, running `resume` before a
resource's dependents come back. A resource nobody had started stays
stopped. A resource whose dependency did not come back in the same `start`
is not started, and stays owed, so a dev server is not brought up against a
database that is still down.

The workspace, trackers, installs, and port claims stay, so the instance
comes back on the same ports. Both commands do nothing and exit 0 when
there is nothing to do, so they can run on every settle and unsettle. Both
exit 1 if any resource failed.

`suspend` never runs for a `project` or `user` resource, which other
instances share, nor for one that never came up. The `supabase` template's
pair is `supabase stop` / `supabase start`, which keeps the data.

### Remove

`newgit remove <name>` stops what is running (dependents first), runs
`[cleanup]` hooks where ownership allows, deletes the workspace (`rm -rf`;
clones have no registration), and archives the binding record.

**The source branch** goes too, when it holds nothing unique, and otherwise
stays with the reason printed. It is deleted only when all of these hold:

- `spawn` created it. A branch that already existed was adopted, and stays
  the user's.
- Nothing has it checked out, in the project checkout or any worktree.
- Its tip is on another ref: a local branch, a remote-tracking ref, a tag,
  or a branch the real remote has right now. newgit's checkpoint refs do
  not count; they are undo history, not where anyone looks for work.

The remote is asked live, only when no local ref has the tip, because a
pull request can be merged and its branch deleted after the last push. If
it cannot be reached, the branch is kept. `remove` re-checks just before
deleting, since cleanup hooks can be slow and two parallel removals of
branches sharing a tip would otherwise each count the other.

Deleting by default is deliberate. `remove` already destroys the
workspace and every resource it can, and a leftover branch is the one
artifact certain to change the next `spawn` of the same name, which would
silently adopt it. A squash-merged branch whose remote branch was deleted
looks unique and is kept: the safe miss, named with the command to finish
the job. `--keep-branch` and `--delete-branch` override.

What `remove` cannot protect is work done since the last checkpoint. When
no ref holds the workspace's `HEAD`, it says that work is lost, so "nothing
unique on the branch" never reads as "nothing lost".

**Checkpoints outlive removal.** They pin what their undo would need, and
newgit never breaks an undo on its own initiative. `remove` reports how
many it kept; `--purge` discards them.

### Cleanup

`newgit cleanup` is garbage collection across the project. `--dry-run`
reports exactly what a real run would do. It:

- **finalizes instances whose workspace is gone**: runs their cleanup
  hooks, deletes their runtime state, and archives their records, freeing
  their names. It leaves their source branches alone: a garbage collector
  deleting branches is a surprise in a way an explicit `remove` is not.
- **deletes unclaimed workspace directories**, but only ones carrying a
  newgit workspace marker or empty. A configured workspace root might point
  somewhere shared, and "newgit deleted a directory it did not create" is
  not worth risking for disk space.
- **clears dead process state** and lock files of instances with no record.
- **prunes tracker revisions nothing references**, and garbage-collects at
  once, so pruned content (possibly a secret) is gone rather than merely
  unreachable. It never deletes a checkpoint record or a revision a
  checkpoint points at, and reports how many it kept for that reason, so
  retained disk is explained.
- **prunes the install store** (see *Installs*).

That conservatism is a dead end for archived instances: `undo` needs a
binding record, so their checkpoints can never be restored, yet they pin
revisions forever. `cleanup --purge-archived` discards them, with their
checkpoint refs. It is opt-in and never touches a live instance. The
ordinary report says how many retained revisions it would free.

### One command per instance at a time

Every command that changes an instance holds that instance's lock from
start to finish (`.newgit/local/locks/<slug>.lock`), and a second command
waits, saying so (#212). Before, two overlapping checkpoints minted the
same id, and an action could save back a record loaded before a parallel
one changed it. The lock is an advisory `flock`, which the kernel drops
when the process exits, so a crash leaves nothing stale. A hook running
under the lock gets `NEWGIT_LOCKED`, so a `newgit` it starts on the same
instance refuses instead of waiting forever for its own ancestor. Records
are written to a temporary file and renamed into place, so no reader sees a
half-written one.

---

## Checkpoints and undo

Checkpointing is the center of newgit for agent work:

```sh
newgit checkpoint feature-a -m "before auth refactor"
# an agent works, and you dislike the result
newgit undo feature-a
```

A checkpoint records, as one coherent record in
`.newgit/checkpoints/<instance>/`:

- **Source**: the workspace's `HEAD`, fetched into the project repository
  under `refs/newgit/checkpoints/<instance>/<id>`, plus a commit of
  uncommitted and untracked work when there is any. That commit is built in
  a throwaway index, leaving the worktree, `HEAD`, and real index alone. A
  checkpoint protects the worktree as it stands, not just what the agent
  remembered to commit.
- **Trackers**: a revision of every bound secret tracker (identical content
  makes no new revision), and the binding itself.
- **Resources**, dependents first: an identity hash, an external handle, or
  a command's output, often deposited into a tracker. Also resolved ports
  and exports, and which processes were running.

A failing `[checkpoint]` command aborts the checkpoint, since a checkpoint
that silently missed a resource is not coherent. The exception is a
resource that is not up: the checkpoint goes ahead without it and names it
on a `degraded:` line.

With a remote, a checkpoint also syncs: the instance's commits are split
into their trackers and pushed, and so are `storage = "remote"` tracker
revisions. A `git push` from the workspace checkpoints first, so what
reaches any remote is always something `undo` can return to.

Undo first checkpoints the current state (reason `before-undo`), so undo is
always undoable and running it twice is redo, as with `jj undo`. Then it
stops running processes, restores the binding, then source (a hard reset,
then the uncommitted work reapplied as uncommitted changes), then tracker
content, then resources by their restore rules in dependency order,
restarting what was running. `undo --only <resource>` restores just that
resource.

A resource's restore is one of `none`, `command`, `recompute` (re-run an
action, `prepare` by default, when the identity changed), or `external`.
`recompute` is correctness for path-dependent artifacts, not an
optimization, and it fills from the install store when it can.

A resource the checkpoint holds no state for (one it went ahead without)
keeps its state when its restore is a `command`. Running the restore with
nothing to restore from would be worse than nothing: every bundled database
template drops or resets the database before reading the dump. Its process
is still stopped and restarted with the rest, reported as `not restored +
restarted`.

### An incomplete undo must say so

A restore command is not transactional. One that rebuilds a schema and then
fails loading the rows leaves its resource in neither state. newgit cannot
fix that; what it must not do is call the instance restored.

So the summary leads with the verdict. A clean undo says `Restored`. One
with any failed resource says `INCOMPLETE: N of M resources restored`,
names the resources that may be partial, writes a recovery record with logs
and a retry command next to the checkpoint, and exits 1. A restore that ran
nothing does not make a resource `ready`: a resource that was failed before
the undo is still failed after it, and the report says what each resource
is left as (#211).

The safety checkpoint taken before an undo is annotated with whether that
undo completed. A pre-undo snapshot before a failed undo is not a real
redo point, and `newgit checkpoints` lists it as `failed-undo` rather than
`before-undo`. The annotation is recorded, not acted on: newgit cannot know
at save time whether the undo will succeed, and deleting the only record of
a state is the one thing checkpoints exist to prevent. The *next* undo's
safety checkpoint captures what a failed undo left behind, so its message
says so (`captured after an incomplete undo; contents may be partial`), and
it records nothing for a resource known to be broken rather than dumping
its rubble.

### An unproven restore must say so

`[checkpoint]` runs the day it is written; `[restore]` runs the day it is
needed, which is by definition the day the instance is already in trouble.
A checkpoint record looks the same whether or not its restore has ever
worked, and a checkpoint wrongly trusted is worse than none, because the
trust is what licenses the risky change taken behind it.

So when a resource's restore runs something that can fail (`command` or
`recompute`) and has never completed on this instance, its checkpoint line
ends `restore never exercised`. The proof is sticky: it answers "has this
ever completed", not "would it now". A `recompute` skipped because the
identity was unchanged does not count, since the command that could fail
never ran.

`newgit checkpoint --verify` proves it on demand: checkpoint, restore that
checkpoint with a real undo, checkpoint again, and compare the resource
state. Agreement is what "verified" means. A mismatch is reported even when
every restore exited 0, since exiting 0 and reproducing the state are
different claims. A mismatch has two possible causes with opposite fixes: a
restore that lands on the wrong state, or a checkpoint that never produces
the same output twice (`pg_dump`'s per-run `\restrict` token). So verify
runs that resource's checkpoint once more with nothing in between, and says
which. For deposits it prints a bounded diff and the command for the full
one. It is destructive and expensive on purpose (a real undo, stopping and
restarting processes), so it is its own flag and says what it is about to
do before it does it.

---

## Leaving the machine

### Export

`newgit export [instance] --to <dir>` writes a normal Git repository:

- **Files Git tracks ship as they stand on disk**, so uncommitted agent
  work is included and ignored junk never is. Rendered paths come from
  `HEAD`. That includes every code tracker's files, whatever its audience:
  this export filters only secret trackers.
- **Secret trackers are filtered by audience, failing closed.** Only
  `public` trackers ship. The rest are withheld and listed, with the flag
  that would ship them. A tracker created without `--audience` is
  `project-devs`, so the default is the safe one. `--include` overrides for
  a path; `--exclude` beats everything.
- **One commit, never history.** Exporting the branch's commits would carry
  any file they ever contained, including withheld ones.

It prints the withheld paths and both caveats (one commit; a path filter is
not concealment) on every run, because this is the command whose output
leaves the machine.

`newgit export --remote <name>` pushes the project's trackers, history and
all, to an ordinary repository defined in `.newgit/exports/<name>.toml`,
typically a GitHub mirror. Only `public` trackers and the ones the target
names ship, so a public mirror and a private one can show different views,
and a history that ever committed a secret tracker's file is refused. It
pushes with your own Git credentials, so GitHub Actions and `gh` keep
working. When the target's branch has moved ahead (a pull request merged on
GitHub), export brings that in first. With `branches = true`, every push
from a workspace exports its branch there too.

### The remote

The newgit remote, at [newgit.dev](https://newgit.dev) or self-hosted from
`remote/`, is how an instance travels. It is a Cloudflare Worker holding one
Git repository per tracker. It gives you:

- GitHub login, project members, and a recorded, revocable token for every
  login, agent, and instance.
- Audience enforced server-side: tokens are minted per tracker, so a reader
  can read only what they may, and `git clone` of the endpoint composes a
  branch from the code trackers you may read.
- Checkpoints that sync, and `spawn --from-checkpoint` to bring one up
  anywhere, with your own copy of per-user trackers.
- `newgit merge [instance] [--into <branch>]`, which merges tracker by
  tracker and moves all of them or none.
- `spawn --private`, an embargo: only you see the instance until `merge`
  publishes it.

[`remote.md`](remote.md) has the design and its status.

---

## Known gaps

Open issues that change what this document can promise:

- **#54: starting is not waiting.** `depends_on` orders `prepare`,
  autostart, and `newgit start`, but a dependent starts once its
  dependency's process exists, not once it answers. `newgit action
  app.start` by hand does not start the database it needs.
- **#64: rendered files are hidden, not filtered.** `--skip-worktree` makes
  hand edits to a rendered file die with the workspace. A clean/smudge
  filter would make them ordinary, committable edits.
- **#83: gitignored secrets do not reach a workspace** unless a tracker owns
  them, and nothing at spawn says so.
- **#85: no reconcile.** A live instance does not pick up a changed
  resource definition (a new port, say), and nothing says it is stale.
- **#87: no preflight.** A definition that calls a missing binary (`pg_dump`
  on a host without Postgres) fails at the first checkpoint or, worse, the
  first restore.
- **#89: a failed `prepare` can destroy its own evidence**, when the tool
  tears down its containers before exiting.
- **#59: port allocation order within a resource is unspecified**, which
  matters when a tool's default ports are consecutive.
- **Install store follow-ups:** sub-path exclusions in `produces` for native
  build output (#91), seeding an entry from the project checkout (#88), and
  build trees that need a relocate step (#128).
- **#107: the push outbox is never garbage-collected**, and the project
  repository's gc can break the objects it borrows.
- **#224: a per-user tracker's `main` reaches the remote only through some
  instance's checkpoint.**
- **#225: restoring a dirty checkpoint can write into a dropped tracker's
  sparse-excluded paths.**
- **#228: on the remote, a tracker handed one definition can claim
  `access`'s**, and lock the owner out.

---

## Not yet

Ideas that are still deferred, and the shape they should take:

- **Command shims.** Agents arrive with reflexes: `git commit`, `pnpm dev`.
  A shim directory on `PATH` could make each one a valid move: *enrich*
  (`git commit` also checkpoints), *inject* (`pnpm dev` gets the instance's
  ports), or *advise* (`git checkout -b` suggests `newgit spawn`), and never
  *emulate*. Each shim runs the real command and announces itself in its
  output, which doubles as onboarding.
- **Env files in the command environment.** `[run] env_files =
  [".env.local"]` or `newgit run --env-file`. It belongs to run policy, not
  trackers: a tracker owns a file's content, and which files become process
  environment is a separate decision.
- **Lazy materialization.** A partial clone first, then FUSE, each
  replacing the clone under the same contract (see *The materializer is
  replaceable*).
- **Network isolation.** Ports are per-instance values today. A reverse
  proxy and branch hostnames come next; Linux namespaces only for untrusted
  agents.
- **Hermetic builds.** Delegated to Nix or Bazel where a project uses them,
  never reimplemented.
- **Richer export.** Deterministic transforms beyond path filters, and
  hunk-level privacy, which needs a patch-level model and is research, not
  a feature.
- **Running on another machine.** [`hosts.md`](hosts.md) sketches `spawn
  --on <host>`.
