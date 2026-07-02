# newgit v1 MVP

*A practical first version of newgit that proves the orchestration thesis without trying to solve every systems problem at once.*

---

## Purpose

The v1 MVP should answer one question:

> Can newgit make parallel agent branches feel effortless by letting a developer define the branch-bound trackers and resources that should move together?

v1 is not a security product, a new VCS, a FUSE filesystem, a Nix replacement, or a privacy-preserving publishing system. It is a local orchestration layer around an existing Git-compatible source tracker.

The product promise is:

> Given a source revision and a set of tracker and resource definitions, newgit can materialize a consistent branch instance with the right workspace, tracked content, running resources, ports, environment exports, and checkpoint/undo behavior.

If that feels good, the larger architecture earns the right to exist.

---

## The Two Primitives

Earlier drafts conflated two different things under the single name "tracker". v1 splits them.

> A **tracker** is a named, versioned lane of file content, with its own audience, propagation, and storage settings.
>
> A **resource** is a lifecycle unit that re-establishes the per-branch state that cannot be carried as content.

The dividing line:

> **Trackers hold state that can travel across space and time** — synced to a remote, restored from a checkpoint. **Resources re-establish the state that can't make that trip** — because it is alive, lives in another system, or is only valid where it was built.

### Trackers

There is no fixed set of trackers and no limit on how many a project defines. `source` is just the default tracker. A user should be able to create a tracker called `jack-env`, set its audience to a single user, and (in a later version) push it to a remote and pull it from another authenticated machine.

Every tracker carries three settings:

- **audience** — who may read it (`public`, `project-devs`, a single user). v1 records this but does not enforce it beyond keeping non-public tracker content out of ordinary Git history.
- **propagation** — how content flows across branches: `rebase` (source-style, changes flow downstream), `pin` (per-branch, no propagation — a different `.env` per branch is normal, not a conflict), or `manual`.
- **storage** — where synced state lives: `local` for v1, `remote` later.

### Resources

Resources exist for exactly three irreducible reasons:

- **Liveness.** A running process cannot be copied, only started. A port cannot be snapshotted, only freshly allocated per instance. A daemon's state can only be captured consistently through the daemon.
- **Externality.** The state lives in another system; the local filesystem holds at most a handle, and an API call is the only interface.
- **Path-dependence.** Installed artifacts (`node_modules`, venvs, native builds) hardcode machine and path. The true state is the identity (the lockfile), and the artifact must be recomputed in place, not copied.

If a thing is purely files that copy correctly, it is a tracker, not a resource.

The two primitives cooperate at exactly one seam: **a resource's checkpoint hook may deposit its output into a tracker** (`pg_dump` → a `db-snapshots` tracker), turning daemon-owned state into carryable, versioned content.

### Why the split matters

Real projects have odd branch-bound things:

- a local Redis namespace
- a Stripe webhook tunnel
- a seeded search index
- a vector database collection
- a generated SDK
- a cloud preview environment
- a mock auth tenant
- a checked-out sibling repo
- a feature flag set

newgit should not need a new internal subsystem for each one. It needs a tracker definition or a resource definition — and an agent reading the config should know, from the noun alone, whether a thing syncs across machines (tracker) or gets re-established on each one (resource).

v1 should ship starter templates for both:

- **Tracker templates:** env-file, file snapshot, SQLite database
- **Resource templates:** install/deps, process/service, command-snapshot database, external resource

But the architecture should treat them all as user-defined instances of the two primitives, not as fixed lanes.

---

## Core Bet

The architecture document says repositories are outputs from a deeper store. v1 should keep that direction, but implement it modestly:

- Source history remains Git-compatible through `jj` or Git.
- Source is special because Git/jj owns history.
- Everything else is a user-defined tracker or resource.
- The "store" is a local metadata directory plus content snapshots and references to existing substrate stores.
- A workspace is a materialized cache, not the source of truth.
- Branch state is a binding record: source revision plus tracker revisions plus resource bindings.
- Exported repositories and branches are derived artifacts, not the internal coordination primitive.

The MVP should prove the binding layer before investing in FUSE, hermetic builds, hunk privacy, or a native remote.

---

## Target User

The first user is an individual developer running multiple local coding agents against the same project.

Assumptions:

- Agents are trusted enough to run as the local user.
- The project already uses Git.
- The developer wants many branch variants active at once.
- Branches need separate state: env files, ports, services, databases, caches, cloud handles, or other project-specific things.
- The main pain is coordination friction, not adversarial security.

Non-assumptions:

- v1 does not assume hostile agents.
- v1 does not promise secret containment from malicious local code.
- v1 does not promise reproducible builds.
- v1 does not need a custom remote.
- v1 does not know every tracker or resource type in advance.

---

## MVP Scope

v1 should include seven things:

1. Branch workspace creation
2. A user-defined tracker model (content lanes with audience, propagation, storage)
3. A user-defined resource model (lifecycle hooks, ports, exports, dependencies)
4. Resource lifecycle actions
5. Exports for env vars, ports, paths, URLs, and opaque state references
6. Checkpoint and undo across source plus trackers plus resources
7. Starter templates for common trackers and resources

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

Every materializer satisfies the same contract (see *Materialization*): a workspace presents a full, real, verifiable Git repo plus every tracker's paths, and nothing else in newgit may care how that presentation is produced. A future `FuseMaterializer` replaces the clone rather than running on top of it — a projection that answers every read exactly as a real clone would. Anything less is emulation.

### No Hardcoded Worldview

v1 should not have bespoke internal systems named `EnvTracker`, `DbTracker`, `InstallTracker`, and so on.

It can ship templates with those names, but the core should see:

```text
TrackerDefinition
TrackerBinding
ResourceDefinition
ResourceBinding
ResourceAction
```

The test for the design is simple:

> Can a user model a tracker or resource the newgit author did not anticipate?

### No Native Remote

v1 should not implement a new remote protocol. It can push and pull through the project's existing Git remote. Tracker `storage = "remote"` is declared in the model but not implemented.

### No Hunk-Level Privacy

v1 should not attempt object/hunk-level tracker partitioning. Path-level tracker membership is enough for early export experiments.

### No Security Claims

v1 should not say branches are isolated from each other, and it should not claim tracker audiences are enforced against local code. `audience` in v1 is a declared intent: it keeps non-public content out of Git history and shapes future remote auth, nothing more. v1 can prevent port collisions and organize services, but it should not claim sandboxing.

### No Nix Reinvention

v1 should not define a derivation language, package store, binary cache, or hermetic builder. If a project already uses Nix, newgit can call it. If it uses pnpm, cargo, uv, or another ecosystem store, newgit can call that.

newgit binds those substrates together. It does not replace them.

---

## Mental Model

newgit manages branch instances.

A branch instance is not just a Git branch. It is:

```text
source revision
+ tracker bindings (which content revision of each tracker)
+ resource bindings (which concrete instance of each resource)
+ exports
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
  resources: Map<ResourceName, ResourceBinding>
  created_at
  updated_at
}

TrackerBinding {
  tracker_name
  definition_rev
  content_rev
  status
}

ResourceBinding {
  resource_name
  definition_rev
  state_ref
  resolved_ports
  resolved_exports
  status
}
```

The workspace directory is disposable. The binding record is not.

---

## The Tracker Primitive

A tracker is a project-defined content lane. It tells newgit:

- which paths (or rendered files) it owns
- who may read it (audience)
- how content flows across branches (propagation)
- where synced state lives (storage)
- how to materialize its content into a workspace
- how to export its content to commands

### Tracker Definition

```text
TrackerDefinition {
  name
  kind          # descriptive, for templates and UI
  audience      # public | project-devs | user:<name>
  propagation   # rebase | pin | manual
  storage       # local | remote (v1: local only)
  paths         # workspace paths this tracker owns
  materialize   # how content lands in a workspace
  exports       # env files, paths
}
```

`kind` is descriptive, not fundamental. The engine cares about paths, materialization, and exports.

### Capture and Restore

Tracker capture is always the same operation: snapshot the tracker's content into the store. In v1 the "store" is `.newgit/snapshots/`, and a capture is a content copy keyed by checkpoint — conceptually a commit, implemented boringly. Restore puts the captured content back into the workspace.

Because tracker state is pure content, capture and restore need no per-tracker modes. If a thing needs a command to capture or restore, it is a resource.

### Tracker Paths and Git

Non-source tracker paths live inside a Git clone, so an agent could `git add`
them into source history. The rule:

> **A path owned by a non-source tracker must be ignored by Git**, unless the
> path is deliberately followed by two trackers.

`newgit tracker add` checks this and appends to `.gitignore`, loudly. The
config loader validates the invariant — content lanes must be disjoint, so no
two trackers fight over the same path at materialize time.

This is "audience keeps content out of Git history by construction" made
concrete. It is not enforcement against a hostile agent force-adding a file;
v1 does not claim that, and a v2 `git commit` shim is the natural place to
catch it.

**Open decision (M2):** dual-tracked paths need a defined precedence — whose
content wins at materialize time, and which capture is authoritative at
checkpoint.

### Source Is the Default Tracker

`source` is a tracker with audience = everyone, propagation = rebase, mechanism = Git/`jj`. It is the one tracker whose history engine is external and non-negotiable (see *Source Tracker* below). Every other tracker's propagation is newgit policy.

---

## The Resource Primitive

A resource is a project-defined lifecycle lane. It tells newgit:

- what resource exists
- how to create or prepare it for a branch
- how to run actions against it
- how to expose values to commands
- how to checkpoint it (often into a tracker)
- how to restore or recompute it
- how to clean it up
- what it depends on

Not every resource implements every hook.

### Resource Definition

```text
ResourceDefinition {
  name
  kind
  ownership
  depends_on
  identity
  ports
  exports
  actions
  checkpoint
  restore
  cleanup
}
```

`ownership` is operational. It tells newgit who owns the concrete resource instance and what cleanup/checkpoint boundary it follows. It is not a security label.

```text
branch      one instance per branch; branch cleanup may delete it
workspace   lives under or depends on the workspace; workspace cleanup may delete it
project     shared by branch instances in this project; per-branch cleanup must not delete it
user        shared outside this project; newgit never deletes it
external    owned outside newgit; cleanup only does what the resource explicitly says
```

The ownership rules should be conservative:

| Ownership | Cleanup | Checkpoint / restore |
|-----------|---------|----------------------|
| `branch` | Branch cleanup deletes the concrete branch instance. | Checkpoint/restore may mutate it freely. |
| `workspace` | Deleted with the workspace. | Recreatable; restore may recompute rather than restore a captured blob. |
| `project` | Never deleted by per-branch cleanup; only explicit project-level teardown may delete it. | Capture should avoid branch-specific mutation unless the resource defines locking or namespacing. |
| `user` | newgit never deletes it, ever. | Capture records identity only; restore is recompute or no-op. |
| `external` | Cleanup runs the defined cleanup command or nothing. | newgit holds a handle and never assumes deletion semantics it did not author. |

`project` and `user` ownership are the easiest places to create destructive defaults by accident. For example, a pnpm store is user-owned: deleting or rewriting it would affect every project on the machine, not just the current branch.

### Lifecycle Hooks

v1 should support a small hook set:

```text
init          create project-level resource metadata
prepare       make the resource ready to use
start         start a long-running process, if any
stop          stop a long-running process, if any
checkpoint    capture branch-local resource state
restore       restore branch-local resource state
status        report resource state
cleanup       remove branch-local resource state
```

The hooks can be command-based in v1. A plugin API can come later.

### Checkpoint Modes

Resources capture in a few modes:

```text
none       no state to capture
hash       record hashes of identity files (lockfiles); restore is recompute
command    run a command that emits state; may deposit into a tracker
external   record an opaque external resource ID
```

The `command` mode's output can target a tracker with `into_tracker`. This is the seam: a Postgres resource's checkpoint runs `pg_dump` and the dump becomes versioned content in a `db-snapshots` tracker, restorable and (later) syncable like any other tracked content.

### Restore Modes

```text
none
command
recompute
external
```

`recompute` is for resources where the captured state is an identity, not a blob. For example, a deps resource records a lockfile hash and reruns the install command when needed. Recompute is correctness for path-dependent artifacts, not just an optimization.

### Exports

Trackers and resources can export values that `newgit run` and resource actions can consume:

```text
env vars
env files
ports
paths
URLs
opaque state refs
```

This is how a database resource can expose `DATABASE_URL`, a service resource can expose `APP_URL`, and a cloud preview resource can expose `PREVIEW_ID`.

### Dependencies

Resources can depend on trackers and other resources:

```text
app-service depends_on ["deps", "runtime-env", "dev-db"]
```

v1 can use a simple topological order:

- materialize trackers and prepare resource dependencies first
- checkpoint dependents first when needed
- restore dependencies before starting dependents
- cleanup dependents before dependencies

The exact ordering rules should stay boring and visible.

---

## Local Layout

v1 can use a simple `.newgit/` directory in the project root:

```text
.newgit/
  config.toml              # committed if it contains no secrets
  trackers/                # committed tracker definitions
    runtime-env.toml
    dev-db.toml
    db-snapshots.toml
  resources/               # committed resource definitions
    deps.toml
    app-service.toml
    postgres-db.toml
  templates/               # committed safe templates
  local/                   # gitignored local overrides
  branches/                # gitignored branch bindings
    feature-a.toml
    feature-b.toml
  snapshots/               # gitignored captured tracker content
  logs/                    # gitignored action logs
  state/                   # gitignored runtime state
```

Workspaces can live outside the repo:

```text
~/.newgit/workspaces/<project>-<hash>/<branch-instance>/
```

`<hash>` is a short hash of the repo root path, so two projects that are both
named `api` do not collide. This is the runtime default; `[workspace].root`
in `config.toml` is optional and omitted from the generated file, since a
committed config should not bake in one user's absolute path.

This avoids cluttering the source repository and makes cleanup simple.

### Committed Config vs Local State

v1 should distinguish the control plane from the data plane.

Committed to Git:

- tracker and resource definitions
- action names and commands
- non-secret defaults
- templates that contain no private values
- documentation of expected trackers and resources

Not committed to Git by default:

- concrete branch bindings
- captured tracker content
- database snapshots
- ports assigned to a branch instance
- action logs
- generated env files
- secrets
- user-specific tracker resolution
- external resource handles, unless explicitly safe

This means v1 can commit newgit project configuration without committing arbitrary state outside source. The source tree still owns source history; `.newgit` committed files describe how branch trackers and resources are orchestrated. Content of trackers whose audience is narrower than the repo's stays out of Git by construction.

### Per-User Tracker Resolution

The architecture should support assigning different concrete tracker inputs to different users, but the assignment should usually happen through local overlays rather than committed person-specific state.

For example, a project can commit one abstract env tracker:

```toml
# .newgit/trackers/runtime-env.toml
kind = "env-file"
audience = "user"
storage = "local"
propagation = "pin"

[resolve]
profile_key = "runtime_env"

[materialize]
render_to = ".env.local"
```

Each user can resolve that tracker differently in a gitignored local file or user-level config:

```toml
[profiles.jack.trackers.runtime-env]
template = "/Users/jack/.config/myapp/env.template"
secret_ref = "op://Private/myapp-dev-env"

[profiles.teammate.trackers.runtime-env]
template = "/Users/teammate/.config/myapp/env.template"
secret_ref = "op://Team/myapp-dev-env"
```

This gives different users different env materialization without putting their env values, secret handles, or branch-local state into Git.

Committed profiles are still useful when they describe roles rather than people:

```toml
[profiles.frontend]
trackers = ["runtime-env"]
resources = ["deps", "app"]

[profiles.fullstack]
trackers = ["runtime-env", "dev-db"]
resources = ["deps", "app", "worker"]
```

The safe default is:

> Commit tracker and resource definitions and role profiles. Keep user resolution and concrete state local.

### Future Remote-Backed Trackers

The local-only v1 rule should not rule out the larger goal: trackers, especially env and secret trackers, should eventually be pushable to a secure newgit remote with per-user authentication. This is what `audience` and `storage` exist for. A tracker called `jack-env` with `audience = "user:jack"` and `storage = "remote"` should sync across Jack's machines and be invisible to everyone else.

The compatibility rule is:

> v1 keeps concrete tracker content out of Git, not out of newgit forever.

For v1, an env tracker might resolve from a local file or existing secret manager:

```toml
# .newgit/trackers/jack-env.toml
kind = "env-file"
audience = "user:jack"
storage = "local"
propagation = "pin"
```

A future version can keep the same tracker and change only the state backend:

```toml
# .newgit/trackers/jack-env.toml
kind = "env-file"
audience = "user:jack"
storage = "remote"
propagation = "pin"

[remote]
auth = "per-user"
encryption = "recipient"
```

That future remote should store tracker content as authenticated, access-controlled newgit data, not as plaintext Git commits. Git can still contain the tracker definition and policy, while the env values live in the newgit remote under per-user auth.

This keeps the upgrade path clean:

- v1: tracker definition in Git, concrete env content local
- later: tracker definition in Git, encrypted/authenticated env content in newgit remote
- never: secret env values committed directly to ordinary Git history

Resource state references should likewise avoid assuming a local file path:

```text
local:snapshots/ckpt_017/runtime-env
external:op://Team/myapp-dev-env
remote:newgit://project/runtime-env/ckpt_017
```

Checkpoint, restore, and materialization should operate on state references through a storage backend. That is the bridge from v1 local state to future secure tracker remotes.

---

## CLI

The CLI should be small, but tracker- and resource-oriented.

Two conventions hold across all commands:

- **Grammar:** branch-instance lifecycle commands are top-level verbs
  (`spawn`, `status`, `run`, `checkpoint`, `undo`, `remove`, `cleanup`);
  definition management uses noun subcommands (`tracker add`, `resource add`).
- **Name inference:** `<name>` is optional when a command runs inside a
  workspace — newgit chose the workspace path, so it can always map cwd →
  branch instance. `<name>` is required only outside a workspace. Agents
  inside a workspace will reflexively type `newgit status`, not
  `newgit status feature-a`; both must work.

### `newgit init`

Initializes `.newgit/` and detects project substrates:

- Git or `jj`
- package manager
- lockfile
- env files
- common dev scripts
- database hints
- likely services

It can offer starter templates, but should not pretend detection is certainty.

### `newgit tracker add <name> --template <template>`

Creates a tracker definition from a starter template.

```sh
newgit tracker add runtime-env --template env-file
newgit tracker add dev-db --template sqlite
newgit tracker add db-snapshots --template file-snapshot
```

### `newgit resource add <name> --template <template>`

Creates a resource definition from a starter template.

```sh
newgit resource add deps --template pnpm
newgit resource add app --template process
newgit resource add postgres-db --template command-snapshot
```

The user can then edit the generated TOML.

### `newgit spawn <name>`

Creates a branch instance:

- creates or selects a source branch/change in the store repo
- rejects a name whose slug collides with an existing instance
- clones the store repo into a fresh workspace directory (see *Materialization*)
- resolves the selected user/profile overlay
- instantiates tracker bindings and materializes tracker content
- instantiates resource bindings
- allocates requested ports
- runs resource prepare hooks
- records initial tracker revisions and resource state references

This is the flagship command.

```sh
newgit spawn auth-refactor --profile fullstack
```

### `newgit run <name> -- <command>`

Runs a command inside the branch instance with:

- tracker and resource exports loaded
- assigned ports exposed as env vars
- workspace as cwd
- logs captured

```sh
newgit run feature-a -- pnpm test
```

### `newgit action <name> <resource>.<action>`

Runs a named resource action.

```sh
newgit action feature-a deps.prepare
newgit action feature-a app.start
newgit action feature-a app.stop
newgit action feature-a postgres-db.migrate
```

Convenience shorthands can come later, but the primitive should be resource actions.

### `newgit checkpoint <name>`

Records the current branch state:

- source snapshot through `jj` or Git, captured by fetching from the workspace clone into the store repo
- content snapshot of every tracker, as one coherent record
- resource checkpoint outputs (identity hashes, external refs, deposits into trackers)
- resolved exports
- port allocations

### `newgit undo <name>`

Restores the branch instance to the previous checkpoint.

For source, delegate to `jj` where possible. For other trackers, restore captured content. For resources, run each restore rule (copy back from tracker, recompute, or external no-op).

### `newgit status`

Shows branch instances with tracker and resource status:

```text
NAME        SOURCE        TRACKERS                RESOURCES                 STATUS
feature-a   abc123        env:r3 db:s17           deps:ready app:running    ok
feature-b   def456        env:r1 db:s18           deps:ready app:stopped    ok
```

### `newgit remove <name>`

Deletes a single branch instance: stops its resources, deletes the workspace
(just `rm -rf` — clones have no registration), and archives the binding
record. Workspaces are disposable; this is the command that proves it, and it
belongs in Milestone 1 — the two-branch success criterion is not really
testable without teardown.

### `newgit cleanup`

Stops resource processes, removes stale workspaces, and prunes unused snapshots according to ownership and cleanup rules. `remove` targets one instance; `cleanup` is garbage collection across everything.

---

## Command Shims

Agents arrive with reflexes: `git commit`, `npm install`, `pnpm dev`. The failure mode is not that they misunderstand newgit — it is that they bypass it, because the workspace looks enough like a normal repo that the old muscle memory fires. Rather than fight those reflexes with documentation, newgit can intercept them: `newgit run` (and the workspace env generally) prepends a shim directory to `PATH`, so every reflex an agent arrives with becomes a valid move.

The design principle is:

> **Interpose, don't emulate.** Run the real command and add newgit side effects. Never make a command secretly do something else — a shim that fakes `git commit` must then fake `git log`, `git status`, and `.git` itself, and an agent that notices a 95%-correct illusion will debug it, decide the repo is corrupt, and start "fixing" it. Everything the environment claims must be verifiable by the tools in that environment.

Shims announce themselves in output rather than hiding:

```text
$ git commit -m "add payment flow"
[main abc1234] add payment flow
[newgit] checkpoint ckpt_018: also captured jack-env, supabase → db-snapshots
[newgit] undo anytime with: newgit undo payment-flow
```

Agents read command output more reliably than any README, so each intercepted command doubles as onboarding — by the third shim message the agent has learned the native interface from the training wheels themselves.

The interception modes:

| Mode | Example | Behavior |
|------|---------|----------|
| Enrich | `git commit`, `npm install` | run the real command, add checkpoint/identity side effects |
| Inject | `pnpm dev` | run the real command through the matching resource, ports and env injected before the process binds |
| Advise | `git checkout -b` | let it happen, print the better newgit move (`newgit spawn`) |
| Emulate | — | never |

`Advise` exists because a few commands semantically diverge: `git checkout -b` expects a new branch in this directory, while `newgit spawn` creates a workspace elsewhere. Silently redirecting would break the agent's model of where it is standing. Intercept effects freely; redirect semantics never.

Shims are a post-MVP milestone (see *What To Defer Until v2*), but `newgit run` should be built so that shimming is only a `PATH` entry away, and shim messages should reuse the same exports/checkpoint machinery — not a second code path.

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

Workspace creation itself belongs to the materializer (a clone of the store
repo — see *Materialization*); the source tracker handles history operations
against the store and workspaces.

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

v1 uses normal directories, materialized per tracker.

The materializer contract:

> A workspace presents a full, real, verifiable Git repo at the binding
> record's revision, plus every tracker's paths materialized — and nothing
> else in newgit is allowed to care how that presentation is produced.

### Workspaces Are Independent Clones, Not Worktrees

newgit does not use `git worktree` or `jj workspace`. Worktree friction is one
of the pain points that motivated this project, and the friction is
structural: worktrees share one gitdir (refs, config, hooks, gc — cross-branch
bleed by construction), they lock a branch to a single worktree, their
lifecycle is registered in the main repo (delete the directory and you owe a
`git worktree prune`), and their `.git`-as-a-file breaks real tools.

Instead, `spawn` materializes a workspace as a complete standalone clone of
the store repo:

- A local `git clone` hardlinks objects; on macOS an APFS `clonefile` of the
  whole repo is effectively instant. Hardlink clones are safe (gc never
  mutates object files in place); `git clone --shared` and `--reference` are
  not, and must never be used.
- The agent inside gets a fully real repo — `.git` is a real directory, and
  `git fsck`, `git log`, anything checks out.
- Deleting a workspace is `rm -rf`. No registration, no prune, no
  coordination. True disposability.
- Nothing is shared between workspaces, so there is nothing to lock and
  nothing to bleed. Two instances of the same base branch is a non-event.

Sync is newgit's job: checkpoint captures source by **fetching from the
workspace clone into the store repo**. Fetch avoids the
push-to-checked-out-branch problem and keeps workspaces passive. The store
only learns about workspace commits when a checkpoint fetches them, so the
undo staging boundary falls out for free: an agent can commit, rebase, and
make a mess inside its clone, and none of it touches the store until a
checkpoint blesses it.

Divergence is policy, not mechanism: a branch bound to an instance is owned
by that instance, and the store treats it as read-mostly. If both the store
and a workspace advance the same branch, newgit warns loudly rather than
hard-refusing — the same safety worktree branch-locking provided, as legible
policy instead of inherited jank.

Clone is only the **source tracker's** materialize step. After it, every
other tracker materializes its paths into the workspace in dependency order
(render an env file, copy a snapshot into place).

### The Path To Projection

Clone and projection are two implementations of the same materializer
contract; a projection replaces the clone rather than running on top of it. A
projected workspace answers every read exactly as a real clone would — the
same repo with lazier I/O, computed on read instead of at spawn. The moment a
projection cuts a corner (a `.git` that fails `fsck`), that's emulation, and
that's the line a `FuseMaterializer` must never cross.

The migration is gradual, and each step keeps the contract fixed:

1. **v1:** eager local clone (hardlinks / APFS clonefile / plain copy).
2. **Later:** Git *partial clone* with the store repo as promisor remote —
   objects fetched on demand through Git's own sanctioned lazy mechanism.
   Big-repo spawn gets fast without newgit touching a filesystem.
3. **v2:** `FuseMaterializer` projects worktree files (checkout-on-read,
   write capture, dedupe across workspaces). A first FUSE version can project
   only the working tree and keep `.git` a small real directory in the
   overlay — Git's lock/rename/fsync behavior on `.git` internals is the
   fussiest thing to reproduce, so it can be deferred within the deferral.

Tracker paths get *easier* under projection, not harder — serving `.env.local`
from a snapshot on read is trivial compared to the Git case.

The key architectural rule is:

> The materializer is replaceable.

Do not let the rest of the code assume that a workspace is the canonical
store. Checkpoint captures source by fetching through Git's interface, not by
reaching into `.git` directly — so it works identically against a clone or a
projection.

---

## Starter Templates

v1 should ship a few templates, but templates are conveniences over the two primitives.

### Env File Tracker

A branch-local env file: pure content, so a tracker.

```toml
# .newgit/trackers/runtime-env.toml
kind = "env-file"
audience = "user"
storage = "local"
propagation = "pin"
paths = [".env.local"]

[materialize]
copy_from = ".newgit/templates/base.env"
to = ".env.local"

[exports]
env_file = ".env.local"
```

This is not a privileged env system. It is a content tracker that exports an env file.

### File Snapshot Tracker

Any branch-local file or directory that should checkpoint and restore as content.

```toml
# .newgit/trackers/generated-sdk.toml
kind = "file-snapshot"
audience = "project-devs"
storage = "local"
propagation = "manual"
paths = ["src/generated"]
```

The same shape covers SQLite, because a (stopped) SQLite database is just a file:

```toml
# .newgit/trackers/dev-db.toml
kind = "sqlite"
audience = "project-devs"
storage = "local"
propagation = "pin"
paths = ["data/dev.sqlite"]

[exports]
DATABASE_URL = "sqlite://data/dev.sqlite"
```

### Install Resource

Dependency preparation through an existing package manager. This is a resource, not a tracker: the installed artifacts are path-dependent and must be recomputed, not copied, and the shared package store is user-owned — newgit must never delete or rewrite it.

```toml
# .newgit/resources/pnpm-store.toml
kind = "external-store"
ownership = "user"

[checkpoint]
mode = "hash"
paths = ["pnpm-lock.yaml"]

[restore]
mode = "none"
```

```toml
# .newgit/resources/deps.toml
kind = "command"
ownership = "workspace"
depends_on = ["pnpm-store"]

[identity]
paths = ["package.json", "pnpm-lock.yaml"]

[actions.prepare]
command = "pnpm install --frozen-lockfile"

[checkpoint]
mode = "hash"
paths = ["package.json", "pnpm-lock.yaml"]

[restore]
mode = "recompute"
action = "prepare"
```

For Nix projects, this template should call Nix rather than imitate it:

```toml
# .newgit/resources/dev-shell.toml
kind = "command"
ownership = "workspace"

[identity]
paths = ["flake.nix", "flake.lock"]

[actions.prepare]
command = "nix develop --command true"

[actions.run]
command = "nix develop --command {{command}}"
```

v1 does not need universal reproducibility. It needs to make resource identity and preparation explicit.

### Process Resource

A branch-local long-running process: alive, so a resource.

```toml
# .newgit/resources/app.toml
kind = "process"
ownership = "branch"
depends_on = ["deps", "runtime-env"]

[ports]
app = { start = 3100, env = "PORT" }

[actions.start]
command = "pnpm dev"
long_running = true

[actions.stop]
signal = "term"

[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
```

This solves port collisions without claiming network isolation.

Optional later improvements:

- local reverse proxy
- branch hostnames
- loopback aliases
- Linux network namespaces

### Command-Snapshot Resource (Postgres)

A daemon-owned database: state can only be captured consistently through the daemon, so a resource — whose checkpoint deposits into a tracker.

```toml
# .newgit/trackers/db-snapshots.toml
kind = "file-snapshot"
audience = "project-devs"
storage = "local"
propagation = "manual"
```

```toml
# .newgit/resources/postgres-db.toml
kind = "command-snapshot"
ownership = "branch"

[actions.prepare]
command = "createdb {{branch.slug}} || true"

[actions.migrate]
command = "pnpm db:migrate"

[checkpoint]
mode = "command"
command = "pg_dump {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "dropdb {{branch.slug}} --if-exists && createdb {{branch.slug}} && psql {{branch.slug}} < {{state_ref}}"

[exports]
DATABASE_URL = "postgres://localhost/{{branch.slug}}"
```

This is enough for Postgres-like workflows without making Postgres a first-class architecture concept — and the dump, once in a tracker, checkpoints and (later) syncs like any other content.

### External Resource

A resource newgit does not own.

```toml
# .newgit/resources/preview.toml
kind = "external"
ownership = "external"

[actions.prepare]
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["PREVIEW_ID", "PREVIEW_URL"]

[checkpoint]
mode = "external"
state_ref = "{{exports.PREVIEW_ID}}"

[cleanup]
command = "cloudctl preview delete {{state_ref}}"
```

This is why resources must be abstract. The world is full of things newgit should orchestrate but not own.

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
- one content snapshot per tracker, written as a single coherent record
- tracker and resource definition revisions
- one checkpoint output per resource (identity hash, external ref, or tracker deposit)
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
content_rev = "snapshots/ckpt_017/runtime-env"

[[tracker_states]]
name = "db-snapshots"
definition_rev = "sha256:..."
content_rev = "snapshots/ckpt_017/db-snapshots"

[[resource_states]]
name = "deps"
definition_rev = "sha256:..."
state_ref = "lock-hash:..."

[[resource_states]]
name = "postgres-db"
definition_rev = "sha256:..."
state_ref = "tracker:db-snapshots@ckpt_017"
```

Do not snapshot every write in v1. Use explicit checkpoints plus source tracker snapshots.

Undo restores in two moves: put every tracker's content back (source via `jj`, others from snapshots), then re-establish resources via their restore rules. Tracker restore is plain content and should not partially fail in interesting ways; if a resource restore hook fails, newgit should report it clearly and leave a recovery record.

---

## Export

v1 can include a modest export command:

```sh
newgit export feature-a --to ../public-export
```

This should produce a normal Git repository or branch from the branch workspace.

Rules:

- path-level include/exclude only, honoring tracker audience as the default filter
- no hunk privacy
- no AST rewriting
- no native remote
- no concealment claims

This keeps the "repositories are outputs" idea alive without making it the first hard dependency.

---

## Configuration

Definitions live one per file, not inlined in `config.toml`:

- `config.toml` holds only `[project]` and (optionally) `[workspace]`.
- Each tracker is a file in `.newgit/trackers/`; each resource is a file in
  `.newgit/resources/`. The definition's name comes from the filename.

One definition per file gives clean diffs, matches what `tracker add` and
`resource add` produce, and avoids the TOML array-of-tables footgun — a
`[[trackers]]` block followed by `[trackers.materialize]` binds by ordering,
which both humans and agents get wrong.

The generated `config.toml` is minimal:

```toml
[project]
name = "myapp"
source = "jj"    # detected at init; "git" fallback; error if neither
```

`[workspace]` is available for overrides but omitted by default (see *Local
Layout* for the runtime defaults):

```toml
[workspace]
root = "/data/newgit-workspaces/myapp"
materializer = "real-dir"
```

A project then looks like:

```text
.newgit/
  config.toml
  trackers/
    runtime-env.toml
    dev-db.toml
  resources/
    pnpm-store.toml
    deps.toml
    app.toml
```

with each definition file shaped as in *Starter Templates* — top-level keys
plus subtables, no `[[trackers]]` wrapper:

```toml
# .newgit/trackers/dev-db.toml
kind = "sqlite"
audience = "project-devs"
storage = "local"
propagation = "pin"
paths = ["data/dev.sqlite"]

[exports]
DATABASE_URL = "sqlite://data/dev.sqlite"
```

The exact config syntax can change. The important part is that content lanes are declared as trackers, lifecycle units as resources, and neither is a hardcoded subsystem.

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
resource registry
binding store
resource action runner
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
TrackerStore.capture(branch, tracker)
TrackerStore.restore(branch, tracker, content_rev)
ResourceRegistry.load()
ResourceRunner.run(branch, resource, action)
ResourceRunner.checkpoint(branch, resource)
ResourceRunner.restore(branch, resource, state_ref)
CheckpointManager.create(branch)
CheckpointManager.restore(branch, checkpoint)
```

Avoid clever generality. The abstractions exist to model ordinary project trackers and resources cleanly, not to build a full distributed workflow engine on day one.

---

## Suggested Build Order

### Milestone 1: Branch Instances

- `newgit init`
- `newgit spawn`
- clone-based workspace creation (see *Materialization*)
- branch metadata files
- `newgit status`
- `newgit remove`

Success criterion:

> I can create two branch instances of the same repo without thinking about paths — and tear one down without thinking about anything.

### Milestone 2: Tracker Definitions And Bindings

- parse tracker definitions
- materialize tracker content into workspaces
- capture and restore tracker content
- show tracker status

Success criterion:

> I can define an env-file tracker and see its content materialize per branch instance.

### Milestone 3: Resources, Actions, Exports, And Ports

- parse resource definitions
- command-based resource actions
- tracker and resource exports loaded into `newgit run`
- deterministic port allocation
- logs per action

Success criterion:

> A resource can request a port, export it, and run a command that uses it.

### Milestone 4: Starter Templates

- env-file tracker template
- file snapshot tracker template
- process resource template
- pnpm install resource template

Success criterion:

> I can model a normal web app without writing TOML from scratch.

### Milestone 5: Checkpoints

- source snapshot integration
- tracker content capture and restore
- resource checkpoint and restore, including `into_tracker`
- explicit checkpoint
- undo

Success criterion:

> I can let an agent work, dislike the result, and restore the previous coherent branch state.

### Milestone 6: Database And External Resources

- SQLite through the file snapshot tracker template
- Postgres through the command-snapshot resource template depositing into a tracker
- one external resource template

Success criterion:

> I can model a branch-local resource that newgit does not natively understand.

### Milestone 7: Basic Export

- path-level export honoring tracker audience
- normal Git output
- no privacy claims

Success criterion:

> A branch instance can produce a clean ordinary Git branch/repo as an output artifact.

---

## What To Defer Until v2

Command shims:

- only after `newgit run` proves the exports and checkpoint machinery they reuse
- start with `git commit` (enrich) and `pnpm dev` / `npm run dev` (inject), the two highest-frequency reflexes
- `advise` shims for branch-creation commands next
- never emulate; every shim passes through to the real command and announces itself in output

FUSE:

- only after real directory materialization is too slow or too leaky
- intermediate step first: Git partial clone with the store repo as promisor remote — lazy objects through Git's own mechanism, no filesystem needed
- add as `FuseMaterializer`, not as a rewrite — it replaces the clone and satisfies the same materializer contract

Hermetic builds:

- only after resource-based install/cache handling proves valuable
- delegate to Nix/Bazel where available

Network isolation:

- start with ports as resource exports
- add reverse proxy and hostnames next
- add Linux namespaces only for untrusted-agent use cases

Remote-backed trackers:

- the `audience`/`storage` fields exist from day one
- per-user authenticated sync only after export semantics are stable
- no security positioning until audited and battle-tested

Projection policy:

- start with path-level export
- add deterministic transforms later
- add coherence shims only when real projects demand them

Hunk privacy:

- research track, not MVP
- requires a patch/overlay semantic model

---

## The v1 Standard

v1 is successful if this workflow feels normal:

```sh
newgit init
newgit tracker add runtime-env --template env-file
newgit tracker add dev-db --template sqlite
newgit resource add deps --template pnpm
newgit resource add app --template process

newgit spawn auth-refactor
newgit action auth-refactor deps.prepare
newgit action auth-refactor app.start
newgit checkpoint auth-refactor -m "before agent"

# agent works in the branch workspace

newgit status
newgit undo auth-refactor
newgit cleanup
```

The magic is not that newgit invented an env system, a DB system, or a package manager. The magic is that a user can teach newgit which trackers and resources matter for a branch, and newgit makes them move together.

That is the MVP.
