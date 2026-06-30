# newgit v1 MVP

*A practical first version of newgit that proves the orchestration thesis without trying to solve every systems problem at once.*

---

## Purpose

The v1 MVP should answer one question:

> Can newgit make parallel agent branches feel effortless by letting a developer define the branch-bound resources that should move together?

v1 is not a security product, a new VCS, a FUSE filesystem, a Nix replacement, or a privacy-preserving publishing system. It is a local orchestration layer around an existing Git-compatible source tracker.

The product promise is:

> Given a source revision and a set of tracker definitions, newgit can materialize a consistent branch instance with the right workspace, resource state, actions, ports, environment exports, and checkpoint/undo behavior.

If that feels good, the larger architecture earns the right to exist.

---

## Core Correction

The MVP should not hardcode "env tracker", "install tracker", or "DB tracker" as product-level concepts.

Those are useful examples, but they should be built from a smaller primitive:

> A tracker is a named branch-bound resource with rules, actions, state capture, restore behavior, exports, and optional dependencies on other trackers.

So v1 should ship with starter tracker templates:

- env-file tracker
- install/cache tracker
- process/service tracker
- port allocation tracker
- file snapshot tracker
- database snapshot tracker

But the architecture should treat them as user-defined tracker instances, not as fixed lanes.

This matters because real projects will have odd resources:

- a local Redis namespace
- a Stripe webhook tunnel
- a seeded search index
- a vector database collection
- a generated SDK
- a cloud preview environment
- a mock auth tenant
- a checked-out sibling repo
- a feature flag set

newgit should not need a new internal subsystem for each one. It should need a tracker definition.

---

## Core Bet

The architecture document says repositories are outputs from a deeper store. v1 should keep that direction, but implement it modestly:

- Source history remains Git-compatible through `jj` or Git.
- Source is special because Git/jj owns history.
- Everything else is a user-defined tracker.
- The "store" is a local metadata directory plus references to existing substrate stores.
- A workspace is a materialized cache, not the source of truth.
- Branch state is a binding record: source revision plus tracker bindings.
- Exported repositories and branches are derived artifacts, not the internal coordination primitive.

The MVP should prove the binding layer before investing in FUSE, hermetic builds, hunk privacy, or a native remote.

---

## Target User

The first user is an individual developer running multiple local coding agents against the same project.

Assumptions:

- Agents are trusted enough to run as the local user.
- The project already uses Git.
- The developer wants many branch variants active at once.
- Branches need separate resource state: env files, ports, services, databases, caches, cloud handles, or other project-specific resources.
- The main pain is coordination friction, not adversarial security.

Non-assumptions:

- v1 does not assume hostile agents.
- v1 does not promise secret containment from malicious local code.
- v1 does not promise reproducible builds.
- v1 does not need a custom remote.
- v1 does not know every resource type in advance.

---

## MVP Scope

v1 should include six things:

1. Branch workspace creation
2. A user-defined tracker model
3. Tracker lifecycle actions
4. Tracker exports for env vars, ports, paths, URLs, and opaque state references
5. Checkpoint and undo across source plus trackers
6. Starter templates for common resources

Everything else is a later milestone.

---

## Non-Goals

### No FUSE

v1 should not mount a filesystem. It should use ordinary directories as materialized workspace caches.

The architecture should still have a materializer boundary:

```text
Store + binding record -> materializer -> workspace directory
```

But the first materializer should be boring:

```text
RealDirMaterializer
```

This keeps editors, file watchers, package managers, test runners, and language servers on familiar ground.

### No Hardcoded Resource Worldview

v1 should not have bespoke internal systems named `EnvTracker`, `DbTracker`, `InstallTracker`, and so on.

It can ship templates with those names, but the core should see:

```text
TrackerDefinition
TrackerBinding
TrackerAction
TrackerState
```

The test for the design is simple:

> Can a user model a resource the newgit author did not anticipate?

### No Native Remote

v1 should not implement a new remote protocol. It can push and pull through the project's existing Git remote.

### No Hunk-Level Privacy

v1 should not attempt object/hunk-level tracker partitioning. Path-level inclusion/exclusion is enough for early export experiments.

### No Security Claims

v1 should not say branches are isolated from each other. It can prevent port collisions and organize services, but it should not claim sandboxing.

### No Nix Reinvention

v1 should not define a derivation language, package store, binary cache, or hermetic builder. If a project already uses Nix, newgit can call it. If it uses pnpm, cargo, uv, or another ecosystem store, newgit can call that.

newgit binds those substrates together. It does not replace them.

---

## Mental Model

newgit manages branch instances.

A branch instance is not just a Git branch. It is:

```text
source revision
+ tracker bindings
+ tracker exports
+ workspace path
+ checkpoint history
```

The binding record is the central object.

```text
BranchInstance {
  id
  name
  source_ref
  source_rev
  workspace_path
  trackers: Map<TrackerName, TrackerBinding>
  created_at
  updated_at
}

TrackerBinding {
  name
  definition_rev
  state_ref
  exports
  status
}
```

The workspace directory is disposable. The binding record is not.

---

## The Tracker Primitive

A tracker is a project-defined resource lane. It tells newgit:

- what resource exists
- how to create or materialize it for a branch
- how to run actions against it
- how to expose values to commands
- how to checkpoint it
- how to restore it
- how to clean it up
- whether it should propagate, pin, inherit, or stay manual

Not every tracker implements every hook.

### Tracker Definition

A tracker definition belongs to the project:

```text
TrackerDefinition {
  name
  kind
  ownership
  depends_on
  propagation
  identity
  ports
  exports
  actions
  capture
  restore
  cleanup
}
```

`kind` is descriptive, not fundamental. It helps templates and UI, but the engine should mostly care about actions, capture rules, restore rules, and exports.

`ownership` is operational. It tells newgit who owns the concrete resource instance and what cleanup/checkpoint boundary it follows. It is not a security label.

```text
branch      one instance per branch; branch cleanup may delete it
workspace   lives under or depends on the workspace; workspace cleanup may delete it
project     shared by branch instances in this project; per-branch cleanup must not delete it
user        shared outside this project; newgit never deletes it
external    owned outside newgit; cleanup only does what the tracker explicitly says
```

The ownership rules should be conservative:

| Ownership | Cleanup | Checkpoint / restore |
|-----------|---------|----------------------|
| `branch` | Branch cleanup deletes the concrete branch instance. | Checkpoint/restore may mutate it freely. |
| `workspace` | Deleted with the workspace. | Recreatable; restore may recompute rather than restore a captured blob. |
| `project` | Never deleted by per-branch cleanup; only explicit project-level teardown may delete it. | Capture should avoid branch-specific mutation unless the tracker defines locking or namespacing. |
| `user` | newgit never deletes it, ever. | Capture records identity only; restore is recompute or no-op. |
| `external` | Cleanup runs the defined cleanup command or nothing. | newgit holds a handle and never assumes deletion semantics it did not author. |

`project` and `user` ownership are the easiest places to create destructive defaults by accident. For example, a pnpm store is user-owned: deleting or rewriting it would affect every project on the machine, not just the current branch.

### Tracker Binding

A tracker binding belongs to one branch instance:

```text
TrackerBinding {
  tracker_name
  branch_instance
  definition_rev
  state_ref
  resolved_ports
  resolved_exports
  last_checkpoint
}
```

The definition says what a resource is. The binding says which concrete instance of that resource belongs to this branch.

### Lifecycle Hooks

v1 should support a small hook set:

```text
init          create project-level tracker metadata
materialize   create branch-local files or references
prepare       make the resource ready to use
start         start a long-running process, if any
stop          stop a long-running process, if any
checkpoint    capture branch-local resource state
restore       restore branch-local resource state
status        report resource state
cleanup       remove branch-local resource state
```

The hooks can be command-based in v1. A plugin API can come later.

### Capture Modes

v1 needs only a few capture modes:

```text
none       no state to capture
hash       record hashes of identity files
copy       copy files/directories into .newgit snapshots
command    run a command that emits a state reference
external   record an opaque external resource ID
```

This is enough to model simple env files, lockfiles, SQLite databases, generated artifacts, local processes, and external preview resources.

### Restore Modes

v1 needs matching restore modes:

```text
none
copy
command
recompute
external
```

`recompute` is for resources where the captured state is an identity, not a blob. For example, a dependency tracker can record a lockfile hash and rerun the install command when needed.

### Exports

Trackers can export values that `newgit run` and other tracker actions can consume:

```text
env vars
env files
ports
paths
URLs
opaque state refs
```

This is how a database tracker can expose `DATABASE_URL`, a service tracker can expose `APP_URL`, and a cloud preview tracker can expose `PREVIEW_ID`.

### Dependencies

Trackers can depend on other trackers:

```text
app-service depends_on ["deps", "runtime-env", "dev-db"]
```

v1 can use a simple topological order:

- materialize and prepare dependencies first
- checkpoint dependents first when needed
- restore dependencies before starting dependents
- cleanup dependents before dependencies

The exact ordering rules should stay boring and visible.

---

## Local Layout

v1 can use a simple `.newgit/` directory in the project root:

```text
.newgit/
  config.toml
  branches/
    feature-a.toml
    feature-b.toml
  trackers/
    runtime-env.toml
    deps.toml
    app-service.toml
    dev-db.toml
  templates/
  snapshots/
  logs/
  state/
```

Workspaces can live outside the repo:

```text
~/.newgit/workspaces/<project>/<branch-instance>/
```

This avoids cluttering the source repository and makes cleanup simple.

---

## CLI

The CLI should be small, but tracker-oriented.

### `newgit init`

Initializes `.newgit/` and detects project substrates:

- Git or `jj`
- package manager
- lockfile
- env files
- common dev scripts
- database hints
- likely services

It can offer starter tracker templates, but should not pretend detection is certainty.

### `newgit tracker add <name> --template <template>`

Creates a tracker definition from a starter template.

Examples:

```sh
newgit tracker add runtime-env --template env-file
newgit tracker add deps --template pnpm
newgit tracker add app --template process
newgit tracker add dev-db --template sqlite
```

The user can then edit the generated TOML.

### `newgit spawn <name>`

Creates a branch instance:

- creates or selects a source branch/change
- creates a workspace directory
- instantiates tracker bindings
- allocates requested ports
- runs tracker materialization hooks
- records initial tracker state references

This is the flagship command.

### `newgit run <name> -- <command>`

Runs a command inside the branch instance with:

- tracker exports loaded
- assigned ports exposed as env vars
- workspace as cwd
- logs captured

Example:

```sh
newgit run feature-a -- pnpm test
```

### `newgit action <name> <tracker>.<action>`

Runs a named tracker action.

Examples:

```sh
newgit action feature-a deps.prepare
newgit action feature-a app.start
newgit action feature-a app.stop
newgit action feature-a dev-db.migrate
```

Convenience shorthands can come later, but the primitive should be tracker actions.

### `newgit checkpoint <name>`

Records the current branch state:

- source snapshot through `jj` or Git
- tracker definition revisions
- tracker capture outputs
- resolved exports
- port allocations

### `newgit undo <name>`

Restores the branch instance to the previous checkpoint.

For source, delegate to `jj` where possible. For other resources, call each tracker's restore rule.

### `newgit status`

Shows branch instances and tracker status:

```text
NAME        SOURCE        TRACKERS                         STATUS
feature-a   abc123        deps:ready app:running db:s17     ok
feature-b   def456        deps:ready app:stopped db:s18     ok
```

### `newgit cleanup`

Stops tracker processes, removes stale workspaces, and prunes unused snapshots according to tracker cleanup rules.

---

## Source Tracker

Source is the one special tracker in v1 because Git compatibility is non-negotiable.

v1 should prefer `jj` if available, because its working-copy model and operation log match the project.

But the implementation should not block on deep `jj` internals. Start by shelling out to `jj` or Git:

- `jj new`
- `jj status`
- `jj describe`
- `jj op log`
- `jj undo`
- Git fallback where needed

Do not fork `jj` for v1.

The first source tracker abstraction can be tiny:

```text
SourceTracker {
  create_branch_instance(name, base)
  snapshot(message)
  current_revision()
  restore(revision)
  status()
}
```

---

## Materialization

v1 uses normal directories.

Possible strategies, in order:

1. Use Git or `jj` workspaces if available.
2. Use filesystem clone/copy primitives where cheap.
3. Fall back to ordinary copy.

Optimization can come later:

- APFS clones on macOS
- reflinks on Linux
- sparse checkout
- hardlinks for immutable files
- FUSE projection

The key architectural rule is:

> The materializer is replaceable.

Do not let the rest of the code assume that a workspace is the canonical store.

---

## Tracker Templates

v1 should ship a few templates, but templates are conveniences over the same primitive.

### Env File Template

This models a branch-local env file.

```toml
[[trackers]]
name = "runtime-env"
kind = "env-file"
ownership = "branch"
propagation = "pin"

[trackers.materialize]
copy_from = ".newgit/templates/base.env"
to = ".env.local"

[trackers.capture]
mode = "copy"
paths = [".env.local"]

[trackers.restore]
mode = "copy"

[trackers.exports]
env_file = ".env.local"
```

This is not a privileged env tracker. It is a file tracker that exports an env file.

### Install Template

This models dependency preparation through an existing package manager.

It is useful to split the shared package store from the workspace-local install action. The shared store is user-owned; newgit must never delete or rewrite it. The workspace install tracker records identity and recomputes the workspace-local artifacts when needed.

```toml
[[trackers]]
name = "pnpm-store"
kind = "external-store"
ownership = "user"
propagation = "recompute"

[trackers.capture]
mode = "hash"
paths = ["pnpm-lock.yaml"]

[trackers.restore]
mode = "none"
```

```toml
[[trackers]]
name = "deps"
kind = "command"
ownership = "workspace"
propagation = "recompute"
depends_on = ["pnpm-store"]

[trackers.identity]
paths = ["package.json", "pnpm-lock.yaml"]

[trackers.actions.prepare]
command = "pnpm install --frozen-lockfile"

[trackers.capture]
mode = "hash"
paths = ["package.json", "pnpm-lock.yaml"]

[trackers.restore]
mode = "recompute"
action = "prepare"
```

For Nix projects, this template should call Nix rather than imitate it:

```toml
[[trackers]]
name = "dev-shell"
kind = "command"
ownership = "workspace"
propagation = "recompute"

[trackers.identity]
paths = ["flake.nix", "flake.lock"]

[trackers.actions.prepare]
command = "nix develop --command true"

[trackers.actions.run]
command = "nix develop --command {{command}}"
```

v1 does not need universal reproducibility. It needs to make resource identity and preparation explicit.

### Process Template

This models a branch-local long-running process.

```toml
[[trackers]]
name = "app"
kind = "process"
ownership = "branch"
depends_on = ["deps", "runtime-env"]
propagation = "pin"

[trackers.ports]
app = { start = 3100, env = "PORT" }

[trackers.actions.start]
command = "pnpm dev"
long_running = true

[trackers.actions.stop]
signal = "term"

[trackers.exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
```

This solves port collisions without claiming network isolation.

Optional later improvements:

- local reverse proxy
- branch hostnames
- loopback aliases
- Linux network namespaces

### File Snapshot Template

This models any branch-local file or directory that should checkpoint and restore.

```toml
[[trackers]]
name = "generated-sdk"
kind = "file-snapshot"
ownership = "workspace"
propagation = "manual"

[trackers.capture]
mode = "copy"
paths = ["src/generated"]

[trackers.restore]
mode = "copy"
```

This same shape can represent SQLite:

```toml
[[trackers]]
name = "dev-db"
kind = "sqlite"
ownership = "branch"
propagation = "manual"

[trackers.capture]
mode = "copy"
paths = ["data/dev.sqlite"]

[trackers.restore]
mode = "copy"

[trackers.exports]
DATABASE_URL = "sqlite://data/dev.sqlite"
```

### Command Snapshot Template

This models resources where state capture and restore happen through commands.

```toml
[[trackers]]
name = "postgres-db"
kind = "command-snapshot"
ownership = "branch"
propagation = "manual"

[trackers.actions.prepare]
command = "createdb {{branch.slug}} || true"

[trackers.actions.migrate]
command = "pnpm db:migrate"

[trackers.capture]
mode = "command"
command = "pg_dump {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"

[trackers.restore]
mode = "command"
command = "dropdb {{branch.slug}} --if-exists && createdb {{branch.slug}} && psql {{branch.slug}} < {{state_ref}}"

[trackers.exports]
DATABASE_URL = "postgres://localhost/{{branch.slug}}"
```

This is enough for Postgres-like workflows without making Postgres a first-class architecture concept.

### External Resource Template

This models a resource newgit does not own.

```toml
[[trackers]]
name = "preview"
kind = "external"
ownership = "external"
propagation = "manual"

[trackers.actions.prepare]
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["PREVIEW_ID", "PREVIEW_URL"]

[trackers.capture]
mode = "external"
state_ref = "{{exports.PREVIEW_ID}}"

[trackers.cleanup]
command = "cloudctl preview delete {{state_ref}}"
```

This is why trackers should be abstract. The world is full of resources newgit should orchestrate but not own.

---

## Checkpoints And Undo

Checkpointing is the emotional center of v1 for agent work.

The developer should be able to say:

```sh
newgit checkpoint feature-a -m "before auth refactor"
```

Then let an agent work, and later say:

```sh
newgit undo feature-a
```

v1 checkpoints should record:

- source revision
- tracker definition revisions
- one captured state record per tracker
- resolved ports
- resolved exports
- action/process status where relevant

The checkpoint format can be simple:

```toml
id = "ckpt_017"
branch = "feature-a"
source_rev = "..."
created_at = "..."
message = "before auth refactor"

[[tracker_states]]
name = "runtime-env"
definition_rev = "sha256:..."
state_ref = "snapshots/ckpt_017/runtime-env"

[[tracker_states]]
name = "deps"
definition_rev = "sha256:..."
state_ref = "lock-hash:..."

[[tracker_states]]
name = "dev-db"
definition_rev = "sha256:..."
state_ref = "snapshots/ckpt_017/dev-db"
```

Do not snapshot every write in v1. Use explicit checkpoints plus source tracker snapshots.

Undo should restore source and tracker state as one coherent operation. If any tracker restore fails, newgit should report the partial restore clearly and leave a recovery record.

---

## Export

v1 can include a modest export command:

```sh
newgit export feature-a --to ../public-export
```

This should produce a normal Git repository or branch from the branch workspace.

Rules:

- path-level include/exclude only
- no hunk privacy
- no AST rewriting
- no native remote
- no concealment claims

This keeps the "repositories are outputs" idea alive without making it the first hard dependency.

---

## Configuration

Example `config.toml`:

```toml
[project]
name = "myapp"
source = "jj"

[workspace]
root = "~/.newgit/workspaces/myapp"
materializer = "real-dir"

[[trackers]]
name = "runtime-env"
kind = "env-file"
ownership = "branch"
propagation = "pin"

[trackers.materialize]
copy_from = ".newgit/templates/base.env"
to = ".env.local"

[trackers.capture]
mode = "copy"
paths = [".env.local"]

[trackers.restore]
mode = "copy"

[trackers.exports]
env_file = ".env.local"

[[trackers]]
name = "pnpm-store"
kind = "external-store"
ownership = "user"
propagation = "recompute"

[trackers.capture]
mode = "hash"
paths = ["pnpm-lock.yaml"]

[trackers.restore]
mode = "none"

[[trackers]]
name = "deps"
kind = "command"
ownership = "workspace"
propagation = "recompute"
depends_on = ["pnpm-store"]

[trackers.identity]
paths = ["package.json", "pnpm-lock.yaml"]

[trackers.actions.prepare]
command = "pnpm install --frozen-lockfile"

[trackers.capture]
mode = "hash"
paths = ["package.json", "pnpm-lock.yaml"]

[trackers.restore]
mode = "recompute"
action = "prepare"

[[trackers]]
name = "app"
kind = "process"
ownership = "branch"
depends_on = ["runtime-env", "deps"]
propagation = "pin"

[trackers.ports]
app = { start = 3100, env = "PORT" }

[trackers.actions.start]
command = "pnpm dev"
long_running = true

[trackers.actions.stop]
signal = "term"

[trackers.exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"

[[trackers]]
name = "dev-db"
kind = "sqlite"
ownership = "branch"
propagation = "manual"

[trackers.capture]
mode = "copy"
paths = ["data/dev.sqlite"]

[trackers.restore]
mode = "copy"

[trackers.exports]
DATABASE_URL = "sqlite://data/dev.sqlite"
```

The exact config syntax can change. The important part is that resources are declared as trackers with common capabilities, not as hardcoded subsystems.

---

## Implementation Shape

v1 can be a Rust CLI, but it should keep hard boundaries:

```text
cli
config
metadata store
source tracker
materializer
tracker registry
tracker binding store
tracker action runner
port allocator
process supervisor
checkpoint manager
exporter
```

The internal APIs should be boring.

```text
BranchManager.spawn(name)
BranchManager.status()
TrackerRegistry.load()
TrackerRunner.run(branch, tracker, action)
TrackerRunner.capture(branch, tracker)
TrackerRunner.restore(branch, tracker, state_ref)
CheckpointManager.create(branch)
CheckpointManager.restore(branch, checkpoint)
```

Avoid clever generality. The abstractions exist to model ordinary project resources cleanly, not to build a full distributed workflow engine on day one.

---

## Suggested Build Order

### Milestone 1: Branch Instances

- `newgit init`
- `newgit spawn`
- workspace creation
- branch metadata files
- `newgit status`

Success criterion:

> I can create two branch instances of the same repo without thinking about paths.

### Milestone 2: Tracker Definitions And Bindings

- parse tracker definitions
- create branch-local tracker bindings
- store tracker state refs
- show tracker status

Success criterion:

> I can define a fake/no-op tracker and see it attached to every branch instance.

### Milestone 3: Actions, Exports, And Ports

- command-based tracker actions
- tracker exports loaded into `newgit run`
- deterministic port allocation
- logs per action

Success criterion:

> A tracker can request a port, export it, and run a command that uses it.

### Milestone 4: Starter Templates

- env-file template
- process template
- pnpm install template
- file snapshot template

Success criterion:

> I can model a normal web app without writing tracker TOML from scratch.

### Milestone 5: Checkpoints

- source snapshot integration
- generic tracker capture
- generic tracker restore
- explicit checkpoint
- undo

Success criterion:

> I can let an agent work, dislike the result, and restore the previous coherent branch state.

### Milestone 6: Database And External Resources

- SQLite through file snapshot template
- Postgres through command snapshot template
- one external resource template

Success criterion:

> I can model a branch-local resource that newgit does not natively understand.

### Milestone 7: Basic Export

- path-level export
- normal Git output
- no privacy claims

Success criterion:

> A branch instance can produce a clean ordinary Git branch/repo as an output artifact.

---

## What To Defer Until v2

FUSE:

- only after real directory materialization is too slow or too leaky
- add as `FuseMaterializer`, not as a rewrite

Hermetic builds:

- only after tracker-based install/cache handling proves valuable
- delegate to Nix/Bazel where available

Network isolation:

- start with ports as tracker exports
- add reverse proxy and hostnames next
- add Linux namespaces only for untrusted-agent use cases

Projection policy:

- start with path-level export
- add deterministic transforms later
- add coherence shims only when real projects demand them

Native remote:

- only after export semantics are stable
- no security positioning until audited and battle-tested

Hunk privacy:

- research track, not MVP
- requires a patch/overlay semantic model

---

## The v1 Standard

v1 is successful if this workflow feels normal:

```sh
newgit init
newgit tracker add runtime-env --template env-file
newgit tracker add deps --template pnpm
newgit tracker add app --template process
newgit tracker add dev-db --template sqlite

newgit spawn auth-refactor
newgit action auth-refactor deps.prepare
newgit action auth-refactor app.start
newgit checkpoint auth-refactor -m "before agent"

# agent works in the branch workspace

newgit status
newgit undo auth-refactor
newgit cleanup
```

The magic is not that newgit invented an env system, a DB system, or a package manager. The magic is that a user can teach newgit which resources matter for a branch, and newgit makes those resources move together.

That is the MVP.
