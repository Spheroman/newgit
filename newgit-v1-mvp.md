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

> A **tracker** is a named, versioned lane of file content, with its own audience, storage, and source-merge policy.
>
> A **resource** is a lifecycle unit that re-establishes the per-branch state that cannot be carried as content.

The dividing line:

> **Trackers hold state that can travel across space and time** — synced to a remote, restored from a checkpoint. **Resources re-establish the state that can't make that trip** — because it is alive, lives in another system, or is only valid where it was built.

### Trackers

There is no fixed set of trackers and no limit on how many a project defines. `source` is just the default tracker. A user should be able to create a tracker called `jack-env`, set its audience to a single user, and (in a later version) push it to a remote and pull it from another authenticated machine.

Every tracker carries three settings:

- **audience** — who may read it (`public`, `project-devs`, a single user). v1 records this but does not enforce it beyond keeping non-public tracker content out of ordinary Git history.
- **merge_with_source** — whether a real source merge should carry this tracker's bound state with it. Env files usually say no; generated code, feature assets, and sub-repo snapshots often say yes.
- **storage** — where synced state lives: `local` for v1, `remote` later.

### Resources

Resources exist for exactly three irreducible reasons:

- **Liveness.** A running process cannot be copied, only started. A port cannot be snapshotted, only freshly allocated per instance. A daemon's state can only be captured consistently through the daemon.
- **Externality.** The state lives in another system; the local filesystem holds at most a handle, and an API call is the only interface.
- **Path-dependence.** Installed artifacts (`node_modules`, venvs, native builds) hardcode machine and path. The true state is the identity (the lockfile), and the artifact is recomputed from it — never copied from an instance whose identity differs. Instances whose identity *agrees* are a different case: the tree is the same by construction, so it is cloned copy-on-write from the install store rather than rebuilt. See *Installs: use a content-addressed store*.

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

v1 ships resource templates for common lifecycle units: `process`, `pnpm`
(install/deps, wired to a shared content-addressed store), `install` (the
same shape for any other package manager, with the lockfile and command left
as `EDIT ME`), `command-snapshot` (a daemon-owned database), and `external`
(a resource another system owns). A template may bring companions it needs —
the resource definitions it `depends_on`, and the tracker lanes its
checkpoint deposits into — created only when absent, so an existing
definition is never overwritten.

Trackers are created through CLI verbs instead of templates: create the lane,
add paths to it, capture content.

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
2. A user-defined tracker model (content lanes with audience, storage, source-merge policy)
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

- which workspace paths it owns
- who may read it (audience)
- whether content should merge along with source changes (`merge_with_source`)
- where synced state lives (storage)

Trackers deliberately rhyme with Git tracking. They are not copy recipes, env
systems, database systems, or command-environment providers. A tracker says
"this lane owns these paths"; capture records the current workspace content in
that lane; projection/restore puts captured lane content back into a workspace.

### Tracker Definition

```text
TrackerDefinition {
  name
  audience      # public | project-devs | user:<name>
  storage       # local | remote (v1: local only)
  merge_with_source # true | false
  paths         # workspace paths this tracker owns
}
```

The tracker file is CLI-managed. Users should not need to hand-edit TOML to
understand or change which paths a tracker owns.

### Capture, Merge, Pull, And Checkout

Tracker capture is always the same operation: snapshot the tracker's content into the store and bind the current branch instance to that rev. In v1 the "store" is a content lane per tracker at `.newgit/snapshots/<tracker>/<rev>/`, where `<rev>` is a content hash — identical captures dedupe, and M5's checkpoints simply reference these revs. Each lane keeps a `LATEST` pointer marking the lane head/default for future instances and explicit pulls.

Capture alone is branch-local. `newgit tracker merge` promotes the current instance's bound tracker rev to the lane head. `newgit tracker pull` checks the lane head into the current instance. `newgit tracker checkout --rev <rev>` checks out a specific captured rev. Checkout clears owned paths first so it reproduces the captured state exactly; before overwriting, the current content is auto-captured, so checkout is always undoable.

A lane starts empty, so a project adopting newgit would otherwise have to
spawn an instance that comes up *without* its env files, copy them in, capture,
and merge — round-tripping content that already sits in the store repo at the
same relative paths. `newgit tracker capture <tracker> --from-store` reads the
tracker's owned paths from the store repo's working tree instead, and sets the
lane head directly: there is no binding record to promote from, and the point
is that the next `spawn` works. Declared paths with nothing behind them are
reported; a tracker with nothing at all on disk is an error rather than an
empty lane head. `newgit tracker track` points at this when the paths it just
added already have content.

Because tracker state is pure content, capture and checkout need no per-tracker modes. If a thing needs a command to capture or restore, it is a resource.

`merge_with_source` does not change what capture means. It answers the global merge question: when a real Git/`jj` source merge is accepted, should this tracker binding be merged with it? If true, the merge/checkpoint shim should promote the branch's tracker rev alongside the source merge. If false, the tracker remains branch-local/user-local unless explicitly merged.

If a tracker has never captured content, including it in a branch instance
projects nothing. This is the same boring rule Git users already know: tracking
a path does not invent content for it.

### Tracker Paths and Git

Non-source tracker paths live inside a Git clone, so an agent could `git add`
them into source history. The rule:

> **A path owned by a non-source tracker must be ignored by Git**, unless the
> path is deliberately followed by two trackers.

`newgit tracker track` checks this and appends to `.gitignore`, loudly. The
config loader validates the invariant — content lanes must be disjoint, so no
two trackers fight over the same path at projection/restore time.

The `.gitignore` append is not sufficient on its own: it is an *uncommitted*
edit in the store worktree, so a workspace clone does not inherit the rule
until the user commits it. In the gap, a lane's content sits in the workspace
as ordinary untracked files that `git add -A` sweeps into source history.
So `spawn` also writes every tracker-owned path into the workspace clone's
`.git/info/exclude`, before any lane content is projected. Audience only
keeps content out of Git *by construction* if the construction reaches every
workspace.

`info/exclude` rather than the workspace's own `.gitignore`: the latter is
tracked content owned by source, and newgit does not rewrite the user's
committed files. This is the one place newgit writes a file under `.git`
instead of going through a Git command, because Git exposes no plumbing that
writes it.

This is "audience keeps content out of Git history by construction" made
concrete. It is not enforcement against a hostile agent force-adding a file;
v1 does not claim that, and a v2 `git commit` shim is the natural place to
catch it.

**Open decision (M2):** dual-tracked paths need a defined precedence — whose
content wins at materialize time, and which capture is authoritative at
checkpoint.

### Source Is the Default Tracker

`source` is a tracker with audience = everyone, storage = Git/`jj`, mechanism = Git/`jj`. It is the one tracker whose history engine is external and non-negotiable (see *Source Tracker* below). Every other tracker uses newgit's capture/merge/pull/checkout policy.

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
  ownership
  depends_on
  identity
  ports
  exports
  render
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

Two ways a value becomes an export:

- **`[exports]`** renders templates at bind time (`APP_URL =
  "http://127.0.0.1:{{ports.app}}"`). Good for anything newgit can compute.
- **`captures`** on an action reads names out of its stdout and merges them
  into the binding's exports. This is how a resource whose handle is minted
  by another system publishes it — newgit cannot compute a preview id, only
  ask for one and remember the answer.

```toml
[actions.prepare]
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["PREVIEW_ID", "PREVIEW_URL"]
```

Two output shapes are accepted, because both are what real commands already
emit: stdout whose first non-whitespace character is `{` is parsed as a flat
JSON object, and anything else is read as `KEY=VALUE` lines. Only declared
names are taken.

**When `captures` is set, stdout belongs to newgit.** Send anything else the
command prints to stderr: a chatty CLI's progress output interleaved with the
values will not parse, and the result is an action that exits 0, reports
`prepare: ok`, marks the resource ready, and publishes an empty handle that
nobody notices until an API call returns 401. A declared name the command did
not emit stays absent rather than becoming an error — a resource may
legitimately publish a handle only on some runs — but newgit warns for each
one, naming the capture and the log to look in. Silence there costs a
debugging cycle, and newgit already knows both the names it wanted and the
stdout it scanned. Captured
values land on the binding record, so they survive the process, reach
`newgit run`, and are available to checkpoint and cleanup hooks. An action
with `captures` runs captured — its output goes to the log rather than the
terminal, because newgit has to read stdout.

Port allocation is deterministic in the useful sense: **an instance's port
never changes once allocated.** Ports are allocated at resource-bind time
(spawn), taking the first port scanning up from the requested `start` that is
neither recorded in any other instance's binding nor OS-unbindable at that
moment, and are persisted in the binding record. The binding records are the
single source of truth — removing an instance frees its ports automatically,
with no separate ledger to drift.

### Render

Ports and exports reach a command two ways: `{{ports.x}}` in its command line
and an env var in `newgit run`. Both assume the tool takes the value on argv
or from the environment. Most tools do not. Supabase reads its ports from
`supabase/config.toml`, Expo from `.env`, Compose from `compose.yaml`, Rails
from `database.yml`, Vite from `vite.config.ts` — a file the project commits
and reviews. Without a first-class answer, every project that hits this writes
the same section-aware config rewriter inside its `prepare` hook, and the
interesting part of the resource stops being `supabase start`.

`[[render]]` is that answer, declared on the resource that owns the values:

```toml
# .newgit/resources/supabase.toml
[[render]]
path = "packages/db/supabase/config.toml"
replace = [
  { find = 'project_id = "faretable"', with = 'project_id = "faretable-{{branch.slug}}"' },
  { find = "port = 54321",             with = "port = {{ports.api}}" },
  { find = "port = 54322",             with = "port = {{ports.db}}" },
  { find = "shadow_port = 54320",      with = "shadow_port = {{ports.shadow}}" },
]
```

#### The committed file is the template

There is no template file. `port = 54321` is not a placeholder, it is
Supabase's working default: someone who clones the repo without newgit and
runs `supabase start` gets a working stack on 54321. Under newgit the same
file gets `54321` → `54400` in this workspace and nowhere else.

This is the whole design, and the reason it is find/replace rather than
rendering a template over the target. A template means a second copy of
`config.toml` under `.newgit/`, kept in sync with upstream's defaults forever,
and a base repo whose real config has been hollowed out into placeholders.
Substituting into the committed file has neither problem: nothing is
duplicated, nothing is degraded, and there is no template-versus-file diff to
reconcile when the branch merges back.

`find` is a **literal string, never a regex.** Regex reintroduces exactly the
"did it match what I meant" doubt that the uniqueness rule below exists to
remove, and it would make the inverse (below) undefined.

#### Replacements are simultaneous, not sequential

> **Every `find` is located in the committed content, and all replacements
> then apply as one batch. A replacement's output is never a match target.**

Rewriting in declaration order — re-matching each rule against the
partially-rewritten text — would make a rule mean different things depending
on what ran before it. A `find` that happens to equal an earlier rule's output
would either report a spurious second match, failing the exactly-once check
below for a duplicate the committed file does not contain, or silently rewrite
that output. Neither is a thing the definition says.

The property to hold onto: **declaration order is cosmetic.** Reordering the
`replace` array cannot change the result, so a reader does not have to
simulate the list to know what it does.

Two rules claiming overlapping text have no batch answer — whichever won would
be an accident of order — so that is refused at render time, naming both
strings.

#### A find must match exactly once

> **Each `find` must match exactly once in the committed content, or the
> render refuses and names the file and the string.**

One rule, doing four jobs:

- **Ambiguity is impossible.** A bare `54321` that also appears in a comment
  or an unrelated key is caught before anything runs, rather than producing a
  file that is wrong in a second place.
- **Drift is loud.** Upstream bumps its default port, or a teammate edits the
  committed config — the `find` stops matching and bind fails with `no match
  for "port = 54321" in packages/db/supabase/config.toml`. This is what
  replaces diffing a template against a file: the check *is* the drift
  detector, it runs on every bind, and it costs nothing.
- **Disambiguation needs no parser.** When two sections share a default value,
  `find` goes multi-line and regains its uniqueness:

  ```toml
  find = """
  [api]
  port = 54321"""
  ```

  newgit never learns what TOML is, which is what lets the same mechanism
  serve `.env`, YAML, and `vite.config.ts`.
- **The inverse stays well-defined** — see below.

For a value that is genuinely repeated (Compose publishing `"3000:3000"`
twice), `count = 2` opts into a declared number of matches. Not `all`: a
declared count keeps failing when the file changes from two occurrences to
three, which is the property worth protecting.

#### Render is a function of committed content, not of the working file

> **A render reads the *committed* content of the path, substitutes, and
> writes the result. It never reads what is currently on disk.**

For a source-owned path that is the blob at `HEAD`; for a tracker-owned path
it is the bound lane rev. Without this rule the second render looks for
`port = 54321`, finds `port = 54400`, and fails — so re-running would have
needed its own state tracking. With it, render is idempotent and pure:
`undo` re-renders from the binding record with no extra machinery, a `pull`
that moves the lane head re-renders from the new content, and the rendered
values can never compound.

Render runs during resource bind, after ports are allocated and after
dependencies are bound, immediately before `prepare` — and again before any
`recompute` restore. Trackers project before resources bind, so a render may
target a tracker-owned path.

#### Available variables

A render template sees **exactly what a command in this instance would see in
its environment**: this resource's `{{ports.*}}`, `{{branch.name}}`,
`{{branch.slug}}`, `{{workspace}}`, and `{{exports.*}}` from every resource
bound so far, in dependency order. Anything narrower and the common
cross-resource case — an Expo `.env` that needs the Supabase resource's URL —
would need a second mechanism.

#### Keeping instance values out of everything downstream

A render writes per-instance values into a path the project otherwise owns.
Four places that value must not escape to:

- **`git status` and `git commit`, for source-owned paths.** newgit marks
  rendered source paths `--skip-worktree` in the workspace clone. Not a
  per-render flag: a render target that shows up as a modification is simply
  broken, and an agent running `git add -A` must not be able to commit this
  instance's port. This is the same job `.git/info/exclude` does for
  tracker-owned paths (*Tracker Paths and Git*), and the same v1 stand-in for
  the projection a v2 FUSE layer does properly — the declaration outlives the
  mechanism.
- **`newgit export`.** `--skip-worktree` does not remove a path from
  `git ls-files`, and export copies the workspace's tracked files *as they
  stand on disk*. Export must therefore take rendered paths from `HEAD`
  rather than from the working tree, or an exported repo ships `port = 54400`.
- **`newgit capture`, for tracker-owned paths.** Here the file cannot simply
  be skipped: the lane is how the `.env` reaches other instances, and the user
  may have legitimately added a key to it. Instead capture **reverses the
  substitution** — rewrites `54400` back to `54321` — and records that. The
  user's real edits survive; the instance's values never enter the lane.

  This is available only because the substitution is literal, and it is the
  strongest argument for this design over a template file: a whole-file
  template cannot be run backwards at all. The inverse needs the same
  guarantee in the other direction — the rendered value must be unique in the
  file too — checked at bind, so the failure surfaces then and not at capture.
- **A checkpoint's uncommitted-state capture.** This one looks like it needs
  nothing — `--skip-worktree` keeps the change uncommitted, so a checkpoint of
  *committed* state never sees it. But `workspace_dirty_commit` builds its
  tree in a throwaway `GIT_INDEX_FILE` seeded from `read-tree HEAD`, and a
  fresh index does not carry the real index's skip-worktree bits. Without
  re-setting them there, `git add -A` sweeps the rendered ports into the
  checkpoint. The one place skip-worktree does not protect on its own.

#### What it costs, and saying so

On a source-owned path, `--skip-worktree` means **real edits to that file in
this workspace are invisible to newgit and die with the workspace.** That is
correct for values newgit generates and wrong for the `[auth]` block someone
adds by hand.

newgit reports this **precisely, not as a caveat.** A warning at bind time
would fire when nothing is wrong yet — at bind the edit does not exist — and
then stay silent at the moment the work is actually lost. Since a render is a
pure function of committed content and the binding record, the expected bytes
of a rendered file are recomputable at any time. So newgit recomputes them and
compares, at the two moments that matter:

- **at checkpoint**, because a checkpoint is the promise that this state can
  be returned to, and a hand edit to a rendered file is the one thing it
  cannot carry;
- **before a re-render** (undo, `tracker pull`, `tracker checkout`), because
  that is the moment the edit is overwritten.

The report names the file and the size of the change —
`packages/db/supabase/config.toml has changes that render will discard (4
line(s) differ)` — and says nothing at all when the file is what the render
produced. *Report missing captures and incomplete undos honestly* applies
here: the generic version of this warning is not honesty, it is noise that
trains people to ignore the specific one. To edit a rendered file for real,
edit it in the store repo.

Tracker-owned render targets do not pay this cost: they are already outside
Git, capture reverses the substitution, and hand edits round-trip.

#### Rules

- A render path must lie inside the workspace. v1 does not render into
  user-level or system config.
- A render path that no tracker owns must be tracked by Git. A path that is
  neither tracked nor tracker-owned has no committed content to render *from*,
  which the purity rule above makes a contradiction, not an edge case.
- Two resources rendering the same path is a config error, refused at load —
  the same disjointness check tracker paths get.
- A `find` that matches zero times, or more times than `count` declares, fails
  the bind rather than rendering partially. The resource is marked `failed`
  and its dependents `blocked`, exactly as a failed `prepare` would.

### Dependencies

Resources can depend on trackers and other resources:

```text
app-service depends_on ["deps", "runtime-env", "dev-db"]
```

`depends_on` is a *lifecycle* claim. Needing another resource's value is a
separate, weaker thing, and it is not declared at all: a `{{exports.<name>}}`
in an `[exports]` value or a `[[render]]` replacement already names what it
needs, so newgit reads the edge out of the template. That gives two orders:

- **bind order** — `depends_on` plus the inferred data edges. Materializing
  trackers, preparing resources, rendering, assembling the environment, and
  restoring all walk this, because a value has to exist before the template
  that reads it renders.
- **lifecycle order** — `depends_on` alone. Checkpoint and cleanup walk it in
  reverse, so needing one string out of a resource never claims anything
  about the order the two are torn down in.

Within that:

- if a resource dependency fails to prepare, leave the branch instance spawned
  but mark dependents `blocked` rather than running their prepare hooks or
  other command actions
- checkpoint dependents first when needed
- restore dependencies before starting dependents
- cleanup dependents before dependencies

An unresolvable graph — a `depends_on` naming something that is neither a
tracker nor a resource, or a cycle — is reported, not raised at load. Commands
that *act* on the graph (`spawn`, `run`, `action`, `checkpoint`, `undo`) refuse
with the offending name. Commands that *build* it (`tracker create`,
`tracker track`, `resource add`) and commands that inspect it (`tracker list`,
`resource list`, `status`) run anyway and print the problem as a warning: they
are how an incomplete graph gets completed, so they must not be the first
casualty of one. `remove` also stays reachable, because teardown must never
depend on the graph holding together.

The exact ordering rules should stay boring and visible. During `spawn`, a
prepare failure does not roll back the workspace, tracker bindings, resource
bindings, allocated ports, or logs; it leaves the instance available for
inspection and repair. A blocked resource can be prepared after its failed
dependency is repaired; command actions such as `start` stay blocked until
then, while signal-only actions such as `stop` remain available.

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
  scripts/                 # committed scripts resource commands call
    db-up.sh               #   as {{scripts}}/db-up.sh
  local/                   # gitignored local overrides
  branches/                # gitignored branch bindings
    feature-a.toml
    feature-b.toml
  snapshots/               # gitignored captured tracker content
  installs/                # gitignored built trees, keyed by identity
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
- scripts those commands call (`.newgit/scripts/`)
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

### Per-User Tracker Content

The architecture should support assigning different concrete tracker content to
different users, but the assignment should happen through tracker storage and
local branch bindings rather than committed person-specific materialization
recipes.

For example, a project can commit one env tracker definition:

```toml
# .newgit/trackers/runtime-env.toml
audience = "user"
storage = "local"
merge_with_source = false
paths = [".env.local"]
```

Each user can then capture their own `.env.local` content into their local
tracker lane. A new branch includes the tracker if its profile asks for it; if
that user has never captured content, the file simply does not appear.

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

> Commit tracker and resource definitions and role profiles. Keep concrete
> tracker content local unless it is deliberately pushed to a tracker remote.

### Future Remote-Backed Trackers

The local-only v1 rule should not rule out the larger goal: trackers, especially env and secret trackers, should eventually be pushable to a secure newgit remote with per-user authentication. This is what `audience` and `storage` exist for. A tracker called `jack-env` with `audience = "user:jack"` and `storage = "remote"` should sync across Jack's machines and be invisible to everyone else.

The compatibility rule is:

> v1 keeps concrete tracker content out of Git, not out of newgit forever.

For v1, an env tracker is just a local content lane:

```toml
# .newgit/trackers/jack-env.toml
audience = "user:jack"
storage = "local"
merge_with_source = false
paths = [".env.local"]
```

A future version can keep the same tracker and change only the state backend:

```toml
# .newgit/trackers/jack-env.toml
audience = "user:jack"
storage = "remote"
merge_with_source = false
paths = [".env.local"]

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
  definition management uses noun subcommands (`tracker create`,
  `tracker track`, `resource add`).
- **Name inference:** `<name>` is optional when a command runs inside a
  workspace — newgit chose the workspace path, so it can always map cwd →
  branch instance. `<name>` is required only outside a workspace. Agents
  inside a workspace will reflexively type `newgit status`, not
  `newgit status feature-a`; both must work.

### `newgit reference`

Prints the definition format: every key in a tracker or resource definition
with its type and default, the checkpoint/restore/ownership tables, and which
template variables are in scope for which hook. It is `crates/newgit/reference/
definitions.md`, compiled into the binary.

This document is a design narrative and does not substitute for a lookup
table. More to the point, it does not travel: `cargo install newgit` puts a
binary and a README on disk and nothing else, so a relative link from the
README to a repository file resolves to nothing on crates.io, on docs.rs, and
in the registry directory. The reference ships inside the thing that reads the
definitions, which is the only copy guaranteed to be wherever the definitions
are. `init`, `tracker create`, and `resource add` each name it, since handing
someone a file to hand-edit is exactly the moment the legal values matter.

### `newgit init`

Initializes `.newgit/` and detects project substrates:

- Git or `jj`
- package manager
- lockfile
- env files
- common dev scripts
- database hints
- likely services

It can offer resource templates and suggested tracker paths, but should not
pretend detection is certainty.

### `newgit tracker create <name>`

Creates an empty tracker lane. Policy can be supplied with flags; defaults are
boring (`audience = "project-devs"`, `storage = "local"`,
`merge_with_source = false`).

```sh
newgit tracker create runtime-env --audience user
newgit tracker create dev-db --audience project-devs
newgit tracker create generated-sdk --merge-with-source
```

### `newgit tracker track <tracker> <path>...`

Adds workspace paths to a tracker lane and appends those paths to `.gitignore`
loudly, unless they are already ignored or deliberately dual-tracked.

```sh
newgit tracker track runtime-env .env.local
newgit tracker track dev-db data/dev.sqlite
newgit tracker track generated-sdk src/generated
```

### `newgit resource add <name> --template <template>`

Creates a resource definition from a starter template.

```sh
newgit resource add deps --template pnpm
newgit resource add app --template process
newgit resource add postgres-db --template command-snapshot
```

The user can then edit the generated resource TOML. Tracker definitions should
normally be edited through the CLI.

Tracker content moves through plumbing subcommands — the same machinery
`checkpoint`/`undo` orchestrate in M5, not a second code path:

```sh
newgit tracker capture <tracker> [instance]      # snapshot content → lane
newgit tracker capture <tracker> --from-store    # seed the lane from the store repo, and set the lane head
newgit tracker merge <tracker> [instance]        # promote this instance's rev to lane head
newgit tracker pull <tracker> [instance]         # pull lane head into this instance
newgit tracker checkout <tracker> [--rev <rev>]  # check out an exact rev (auto-saves current first)
newgit tracker list
```

`[instance]` is inferred when run inside a workspace.

### `newgit spawn <name>`

Creates a branch instance:

- creates or selects a source branch/change in the store repo
- rejects a name whose slug collides with an existing instance
- clones the store repo into a fresh workspace directory (see *Materialization*)
- resolves the selected user/profile overlay
- instantiates tracker bindings and projects captured tracker content
- instantiates resource bindings
- allocates requested ports
- runs resource prepare hooks
- records initial tracker revisions and resource state references

This is the flagship command.

```sh
newgit spawn auth-refactor --profile fullstack
```

### `newgit run [name] -- <command>`

Runs a command inside the branch instance with the environment assembled
from:

1. resource exports, in dependency order
2. port env vars (`PORT=3107`)
3. `NEWGIT_BRANCH`, `NEWGIT_WORKSPACE` context vars

A name belongs to exactly one declaration. Two resources claiming one name
is a graph problem reported when the graph loads, not a last-one-wins
resolution discovered as a missing variable in a subprocess.

The workspace is the cwd; output is captured to `.newgit/logs/` as well as
the terminal.

```sh
newgit run feature-a -- pnpm test
```

Template variables available in exports and action commands are kept
minimal: `{{ports.<name>}}`, `{{branch.name}}`, `{{branch.slug}}`,
`{{workspace}}`, `{{scripts}}`. Checkpoint and restore commands additionally
see `{{exports.<name>}}`, `{{snapshot.path}}` (the staging dir for
`into_tracker` deposits), and `{{state_ref}}` (the checkpointed state
reference) — nowhere else. `newgit reference` prints the full scope table.

### Where A Resource's Script Lives

A resource definition is read from the store, but anything its commands shell
out to is read from the workspace, where it is subject to source
materialization. That split the two halves of one definition across different
rules: the TOML was editable in place, while the script it called had to be
committed before the first spawn could see it. Iterating on a `prepare` meant
committing every attempt, or copying the script into the workspace by hand
between runs.

`{{scripts}}` resolves to `.newgit/scripts/` **in the store**, the same rule
the definitions in `.newgit/resources/` already follow:

```toml
[actions.prepare]
command = "{{scripts}}/db-up.sh {{branch.slug}}"
```

Both halves of the definition now live under one rule — edit either and the
next `newgit action` picks it up, with nothing to commit first. The directory
is control plane and belongs in Git alongside `config.toml`, `trackers/`, and
`resources/`; a teammate or CI without it cannot bind the resource. `init`
writes a `README.md` there, both because Git will not track an empty
directory and because that is where someone looks first.

Scripts run with the workspace as their working directory, so relative paths
inside one address the instance being prepared. Nothing is copied into the
workspace — the script executes from the store.

This does not change where a *project's* own scripts live. Something the
application itself runs stays in the project tree, is read from the
workspace, and must be committed before a spawn that calls it. The rule is
about ownership: `.newgit/scripts/` is for scripts that exist to serve a
resource definition.

### `newgit action <resource>.<action> [instance]`

Runs a named resource action. The instance comes last (and is inferred
inside a workspace) — an agent standing in its workspace types
`newgit action app.start`. This deliberately deviates from an earlier
`action <name> <resource>.<action>` draft for consistency with the
`tracker` subcommands.

```sh
newgit action deps.prepare feature-a
newgit action app.start          # inside a workspace
newgit action app.stop
newgit action postgres-db.migrate
```

Actions with `long_running = true` run under a minimal PID-file supervisor:
the process starts detached in its own process group with output to a log
file under `.newgit/logs/`, the PID is recorded in `.newgit/state/`, `stop`
sends the action's configured signal (default TERM) to the group, and
`status` checks liveness. No daemon, no restart policy — boring.

Convenience shorthands can come later, but the primitive should be resource actions.

### `newgit checkpoint [instance] [-m <message>]`

Records the current branch state (instance inferred inside a workspace, like
the other subcommands):

- source snapshot through Git, captured by fetching from the workspace clone
  into the store repo (`refs/newgit/checkpoints/<slug>/<id>`); the store
  branch ref is blessed to the workspace head, warning loudly on divergence
- **uncommitted and untracked state too**, as a dangling commit on top of
  HEAD (built via a throwaway index — worktree, HEAD, and real index are
  untouched); a checkpoint protects the worktree as it stands, not just what
  the agent remembered to commit
- content snapshot of every tracker, as one coherent record
- resource checkpoint outputs, dependents first (identity hashes, external
  refs, deposits into trackers); a failed checkpoint command aborts the
  checkpoint loudly — a checkpoint that silently missed a resource is not
  coherent
- resolved exports, port allocations, and which processes were running

### `newgit undo [instance] [--to <ckpt_id>]`

Restores the branch instance to a checkpoint (the latest unless `--to`).

Before restoring anything, the current state is checkpointed
(`reason = "before-undo"`), so undo is always undoable and running `undo`
twice is redo — same shape as `jj undo`. Then: running processes are
stopped, source is restored through Git (hard reset + clean of non-ignored
untracked files, then the dirty snapshot reapplied as uncommitted changes),
tracker content is restored exactly, and resources run their restore rules
in dependency order, restarting what was running. A resource restore failure
does not abort the rest: it is collected into a recovery record next to the
checkpoint (`<id>.recovery.toml`) with logs and a retry command, and the
resource is marked failed.

Source restore is pure Git in v1; `jj` delegation can come with the jj
substrate work.

#### An Incomplete Undo Must Say So

A restore command is not transactional. One that does two things — rebuild a
schema, then load the captured rows — and fails on the second leaves its
resource in neither the pre-undo state nor the checkpoint state. newgit
cannot fix that; non-transactional resources are inherent. What it must not
do is describe the instance as restored anyway.

So the summary leads with the verdict. A clean undo reports `Restored`; one
with any failed resource reports `Undo of <instance> ... INCOMPLETE: N of M
resources restored`, names the resources that may be in a partial state, and
exits non-zero. Saying `Restored` on the first line and `FAILED` on the
fourth sends the reader to look at their script instead of at the resource.

The safety checkpoint is annotated once the undo finishes, with
`undo_completed = true|false`. A pre-undo snapshot taken before an undo that
failed captures a state the instance never cleanly left: it is not a redo
point, and three failed attempts otherwise leave three entries
indistinguishable from checkpoints a human chose to keep, each pinning its
tracker revs. It is recorded rather than acted on — newgit cannot know at
save time whether the undo will succeed, and deleting the only record of a
state is the one thing checkpoints exist to prevent. Releasing those revs is
a `cleanup` concern.

The annotation alone is not enough, because it lands on the *wrong*
checkpoint to warn a reader off. `undo_completed` marks the pre-undo
snapshot that an undo attempt *preceded*; the state a failed undo actually
leaves behind is captured by the *next* undo's safety checkpoint, which by
construction is a legitimate redo point (the undo it preceded succeeded) and
so is never marked. Its message — `state before undo to ckpt_001` —
describes when it was taken, which a reader takes as a description of what
is in it. When the instance's last operation was an incomplete undo, that
message says so: `state before undo to ckpt_001 (captured after an
incomplete undo; contents may be partial)`. The same reasoning applies per
resource: a safety checkpoint does not re-run a resource's checkpoint
command against a resource whose own last restore already failed — it is
known-broken, not merely unobserved, and dumping it produces exactly the
rubble the message warns about, for a redo point nobody is likely to want.
That resource's entry records `mode = "none"` instead, and a warning names
it. Explicit checkpoints are unaffected either way: a person asked for that
one on purpose.

### `newgit checkpoints [instance]`

Lists an instance's checkpoints (id, created, reason, source rev, message) —
what makes `undo --to` usable. The reason distinguishes `explicit` (a human
named it), `before-undo` (auto-saved, and a real redo point), and
`failed-undo` (auto-saved before an undo that did not complete).

### `newgit status`

Shows branch instances with tracker and resource status:

```text
NAME        SOURCE        TRACKERS                RESOURCES                 STATUS
feature-a   abc123        env:r3 db:s17           deps:ready app:running    ok
feature-b   def456        env:r1 db:s18           deps:ready app:stopped    ok
```

`newgit status <instance> --path` prints that instance's workspace path on
stdout and nothing else. A workspace path is the one piece of newgit state
that scripts, editor integrations, and READMEs actually need
(`W=$(newgit status auth-refactor --path)`), and the alternatives — parsing
the table or reading `.newgit/branches/<name>.toml` — make the store layout
someone else's API. Warnings still go to stderr, so stdout stays a path.

### `newgit remove <name>`

Deletes a single branch instance: stops its resources, deletes the workspace
(just `rm -rf` — clones have no registration), and archives the binding
record. Workspaces are disposable; this is the command that proves it, and it
belongs in Milestone 1 — the two-branch success criterion is not really
testable without teardown.

Checkpoints outlive removal: they pin the tracker revs their undo would need,
and newgit never breaks an undo on its own initiative. That leaves retained
disk for an instance nobody can undo any more, so removal reports how many
checkpoints it kept, and `--purge` discards them instead — the one flag that
says the undo will never be wanted.

### `newgit cleanup`

Garbage collection across everything; `remove` targets one instance. Takes
`--dry-run`, which reports exactly what a real run would remove. Four jobs:

- **Finalizes instances whose workspace is gone.** Such an instance cannot
  run, checkpoint, or undo, and its name stays taken — so cleanup runs its
  resource cleanup hooks, deletes its runtime state, and archives its binding
  record, which frees the name for `newgit spawn` again.
- **Deletes unclaimed workspace directories** — a failed spawn, or a record
  archived while its directory survived. Narrower than "unclaimed": a
  directory is removed only if it carries a newgit workspace marker (proof
  newgit created it) or is empty. `[workspace] root` is user-configurable and
  might point somewhere shared, and "newgit deleted a directory it did not
  create" is not a failure mode worth risking to reclaim disk. Anything else
  is reported and left alone.
- **Clears dead process state**: PID files whose process group has exited,
  and state directories belonging to no live instance.
- **Prunes tracker lane revs nothing references**, plus staging directories a
  crashed capture left behind. Roots are the surviving instances' bindings,
  every checkpoint record (including archived instances'), and each lane's
  head. It never deletes a checkpoint record, and never a rev a checkpoint
  still points at — the count of revs retained for that reason is reported,
  so kept disk is explained rather than mysterious.

#### Releasing an Archived Instance's History

That conservatism is right for a live instance and a dead end for an archived
one: once the binding record is gone, `newgit undo` cannot reach those
checkpoints at all, yet they keep pinning revs forever. `newgit cleanup
--purge-archived` discards the checkpoint logs of instances that have no
binding record — and their `refs/newgit/checkpoints/<slug>/*` store refs, so
the source commits stop being rooted too — which lets the same pass prune what
those logs were the only claim on. A purging `--dry-run` reports exactly what
the real run would remove, including those revs.

It stays opt-in, and it never touches a live instance's checkpoints whatever
the flag says. Deleting the only record of a state is the one thing
checkpoints exist to prevent, so it happens when the user says so and not
because a garbage collector inferred it. To make that reachable rather than
folklore, the ordinary `cleanup` report says how many of the pinned revs are
held only by archived instances, and `remove` names the flag when it keeps
checkpoints behind.

### Resource Cleanup Hooks

A resource's `[cleanup] command` runs during both `newgit remove` and
`newgit cleanup`, dependents before dependencies, while the workspace still
exists. Without this, a resource newgit does not own — a cloud preview, a
database — would outlive every trace of the instance that asked for it.

Ownership decides whether a hook may run at all, not the presence of a
command:

| Ownership | Per-branch teardown |
|-----------|---------------------|
| `branch`, `workspace` | hook runs |
| `external` | hook runs — exactly the defined command and nothing else |
| `project`, `user` | **never**; shared beyond this instance, skipped loudly |

A hook's command may use `{{state_ref}}` (the most recent checkpointed state
reference for that resource, which is how `cloudctl preview delete
{{state_ref}}` gets its argument) and `{{exports.<name>}}` from the binding.
If rendering leaves any `{{...}}` unresolved, the hook is **refused, not
run**: a destructive command with a literal placeholder is not a no-op, it is
a wrong argument. Rendering elsewhere deliberately leaves unknown variables
verbatim so misconfiguration is visible; cleanup, `[[render]]`, and
`[exports]` are the places that must fail closed instead — each writes
something durable that outlives the command that got it wrong.

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

Clone is only the **source tracker's** projection step. After it, every
included tracker projects its captured paths into the workspace. If a tracker
has no captured content, it projects nothing.

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

## Common Definitions

### Env File Tracker

A branch-local env file: pure content, so a tracker.

```sh
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit tracker capture runtime-env --from-store   # the file is already here
```

`--from-store` is the adoption path: it seeds the lane from the env file the
project already has, so the first `spawn` comes up with it. Without existing
content, capture from an instance workspace once you have written one there.

This is not a privileged env system. It is a content tracker that owns an env
file. Loading that file into `newgit run` is a separate command-environment
policy, not part of tracking.

One lane, one file, but each instance needs its own port inside it. That is
`[[render]]`, declared on the resource that owns the port:

```toml
# .newgit/resources/supabase.toml
[[render]]
path = ".env.local"
replace = [
  { find = "SUPABASE_URL=http://127.0.0.1:54321",
    with = "SUPABASE_URL=http://127.0.0.1:{{ports.api}}" },
]
```

The lane keeps `54321`, the instance gets its own, and `newgit capture`
reverses the substitution before recording — so a key you add to `.env.local`
reaches the lane and this instance's port does not.

The resulting tracker file is intentionally small:

```toml
# .newgit/trackers/runtime-env.toml
audience = "user"
storage = "local"
merge_with_source = false
paths = [".env.local"]
```

### File Tracker

Any branch-local file or directory that should checkpoint and restore as content.

```sh
newgit tracker create generated-sdk --merge-with-source
newgit tracker track generated-sdk src/generated
```

The same shape covers SQLite, because a (stopped) SQLite database is just a file:

```sh
newgit tracker create dev-db
newgit tracker track dev-db data/dev.sqlite
```

### Install Resource

Dependency preparation through an existing package manager. This is a resource, not a tracker: the installed artifacts are path-dependent and must be recomputed, not copied, and the shared package store is user-owned — newgit must never delete or rewrite it.

#### What Independence Costs

Every branch instance gets its own installed artifacts. That is what makes
instances independent — two branches with different lockfiles must not share
a dependency tree, or one branch's install silently rewrites the other's
dependencies — and it is why installs are resources rather than trackers.

What that independence *costs* is set by the package manager, not by newgit,
and the right unit to share is the package store rather than the installed
tree:

- **pnpm** hardlinks each package from one content-addressed store into every
  tree that needs it, so a tenth instance adds directory entries rather than
  gigabytes. (Hardlinks cannot cross filesystems: if `[workspace] root` is on
  a different volume from the store, pnpm falls back to copying.) Yarn PnP
  avoids the tree entirely. `uv` does the same for Python.
- **npm** expands a full copy per instance. Its cache holds tarballs, so
  `npm ci` re-expands every time; ten instances of a monorepo means ten full
  copies of `node_modules`. `pip` into a per-instance venv behaves the same
  way.

This is the one place where newgit multiplies an existing cost instead of
absorbing it, so it is worth stating plainly rather than leaving for an
adopter to discover by watching a disk fill. newgit has no lever here — a
shared installed tree is the thing that would be wrong.

```toml
# .newgit/resources/pnpm-store.toml
ownership = "user"

[checkpoint]
mode = "hash"
paths = ["pnpm-lock.yaml"]

[restore]
mode = "none"
```

```toml
# .newgit/resources/deps.toml
ownership = "workspace"
depends_on = ["pnpm-store"]

[identity]
paths = ["package.json", "pnpm-lock.yaml"]
produces = ["node_modules"]
key_command = "node -v && uname -sm"

[actions.prepare]
command = "pnpm install --frozen-lockfile"

[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
```

`produces` names the tree `prepare` builds, which puts this resource in the
**install store**: the first instance at a given identity installs and its
tree is published to `.newgit/installs/<resource>/<key>/`; every later
instance at that key is filled from it with a copy-on-write clone and does not
run `prepare` at all.

The key is the content of `paths`, the stdout of `key_command`, and the
definition file itself. The first is what the resource declares it is derived
from. The second is what that cannot see — a lockfile hash describes what was
*asked for*, and install scripts compile against a platform and a toolchain it
never names. The third is because the *command* is an input to the tree as
much as its inputs are: edit `prepare` and the next instance must rebuild
rather than be handed what the old command built.

Three properties make this safe rather than the shared-tree bug the design
exists to prevent:

- **Copies fork on write.** Clones are copy-on-write (`clonefile` on APFS,
  `--reflink` on btrfs/XFS), never hardlinks. `node_modules` is not read-only
  after install — native builds write into it — and under hardlinks one
  instance's build would rewrite every other instance's tree. Where the
  filesystem cannot clone, newgit takes a full copy and says which it did:
  same behaviour everywhere, worse performance.
- **A tree that names its own workspace is never shared.** A postinstall that
  bakes in its absolute location produces a tree copy-on-write cannot fix, so
  admission scans for it and declines, naming the file. That resource simply
  keeps installing per instance.
- **Entries are a cache, not data.** `newgit cleanup` drops any entry no live
  instance's identity still keys to, per resource, along with anything an
  interrupted publish left behind. The tree is rebuildable from the inputs
  that key it, so the cost of being wrong is an install.
- **`prepare` is the only command an entry stands in for.** A spawn runs
  `prepare` and nothing else, so a definition where something else builds the
  tree — a `[restore] action` naming another action, a `long_running` or
  command-less `prepare`, or one declaring `captures` it would no longer
  publish — is refused rather than left to silently never fill.

A `recompute` restore consults it too. Undo restores source before resources,
so the workspace already holds the checkpoint's inputs by then and the store's
entry under that key is the tree the rebuild would produce — the existing tree
is moved aside, the clone lands, and only then is the old one discarded.
`newgit undo --force-recompute` bypasses the store *and drops the entry*: that
flag means the identity is not to be trusted to describe the tree, and a cache
keyed on the identity is under the same suspicion.

`newgit action <resource>.prepare` always runs the command — the store stands
in for a *spawn*, never for a command someone asked for by name — but
publishes what it built.

Declared `produces` paths are added to each workspace's `.git/info/exclude`
alongside tracker-owned paths. Derived content is not source, and without the
rule a checkpoint's `git add -A` would sweep an entire install into source
history.

For Nix projects, this template should call Nix rather than imitate it:

```toml
# .newgit/resources/dev-shell.toml
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
audience = "project-devs"
storage = "local"
merge_with_source = false
paths = []
```

```toml
# .newgit/resources/postgres-db.toml
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

v1 checkpoints record:

- source revision, plus a dirty-state commit when the worktree had
  uncommitted or untracked changes
- one content snapshot per tracker (a lane rev — identical content dedupes),
  written as a single coherent record
- tracker and resource definition revisions
- one checkpoint output per resource (identity hash, external ref, or tracker
  deposit), captured dependents-first
- resolved ports and exports, and whether each long-running resource was
  running

The format, as implemented (`.newgit/checkpoints/<slug>/ckpt_NNN.toml`):

```toml
id = "ckpt_017"
branch = "feature-a"
created_at = "..."
message = "before auth refactor"
reason = "explicit"                  # or "before-undo" (undo's safety checkpoint)
undo_completed = true                # before-undo only: was that undo complete?

[source]
head_rev = "..."                     # workspace HEAD
dirty_rev = "..."                    # optional: dangling commit of uncommitted state
store_ref = "refs/newgit/checkpoints/feature-a/ckpt_017"

[[tracker_states]]
name = "runtime-env"
definition_rev = "sha256:..."
content_rev = "3bd17e8ba807"         # lane rev under .newgit/snapshots/runtime-env/

[[resource_states]]
name = "deps"
definition_rev = "sha256:..."
mode = "hash"
state_ref = "hash:9921aa04d2e1"
was_running = false

[[resource_states]]
name = "postgres-db"
definition_rev = "sha256:..."
mode = "command"
state_ref = "tracker:db-snapshots@77e10b2c4451"
state_path = ".../.newgit/snapshots/db-snapshots/77e10b2c4451/db.sql"
was_running = false
```

`state_path` is how the seam closes at restore time: when a checkpoint
command deposited into a tracker and echoed a path under `{{snapshot.path}}`,
that path is rebased onto the lane rev directory, and the restore command's
`{{state_ref}}` renders to it.

Checkpoint refs keep the commits alive and checkpoints reference lane revs,
so lane pruning (a `cleanup`/M6 concern) must never delete a rev a
checkpoint still points at.

Do not snapshot every write in v1. Use explicit checkpoints plus source tracker snapshots.

Undo restores in two moves: put every tracker's content back (source through
Git's public interface, others from lane snapshots), then re-establish
resources via their restore rules. Tracker restore is plain content and
should not partially fail in interesting ways; if a resource restore hook
fails, newgit reports it clearly and leaves a recovery record next to the
checkpoint.

---

## Export

```sh
newgit export feature-a --to ../public-export
newgit export feature-a --to ../public-export --include data/seed.sql --exclude notes/
```

This produces a normal Git repository from the branch workspace. Rules
unchanged from the original intent: path-level include/exclude only, honoring
tracker audience as the default filter; no hunk privacy, no AST rewriting, no
native remote, no concealment claims.

As implemented:

- **Source always ships**, because source's audience is everyone. Content is
  the workspace's Git-tracked files *as they stand on disk*, so uncommitted
  agent work is included and ignored junk never is. The one exception is
  rendered paths (*Render*), which are taken from `HEAD`: on disk they hold
  this instance's ports, and `--skip-worktree` does not keep them out of
  `git ls-files`.
- **Tracker audience is the default filter, and it fails closed.** Only
  `audience = "public"` lanes are included. `project-devs` and `user` lanes
  are withheld and listed, with the flag that would ship them. A tracker
  created without `--audience` is `project-devs`, so the boring default is
  the safe one.
- **`--include <path>`** overrides audience for exactly that path (it may
  also name something no tracker owns and Git does not track — a build
  output). **`--exclude <path>`** is applied last and beats everything,
  including `--include`.
- **One commit, never history.** Exporting the branch's commits would carry
  any file those commits contain, including the content the audience filter
  just withheld. The export is a single commit on a branch named after the
  instance, in a fresh repository with no remotes and no `refs/newgit/*`.
- The destination must be empty or nonexistent, and an export that would
  contain nothing is an error rather than an empty repository.

The CLI prints both the withheld paths and the two load-bearing caveats (one
commit; a path filter is not concealment) on every run, because this is the
one command whose output leaves the machine.

This keeps the "repositories are outputs" idea alive without making it the first hard dependency.

---

## Configuration

Definitions live one per file, not inlined in `config.toml`:

- `config.toml` holds only `[project]` and (optionally) `[workspace]`.
- Each tracker is a file in `.newgit/trackers/`; each resource is a file in
  `.newgit/resources/`. The definition's name comes from the filename.

One definition per file gives clean diffs, matches what `tracker create`,
`tracker track`, and `resource add` produce, and avoids the TOML
array-of-tables footgun.

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

with each tracker definition file kept deliberately small:

```toml
# .newgit/trackers/dev-db.toml
audience = "project-devs"
storage = "local"
merge_with_source = false
paths = ["data/dev.sqlite"]
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

All seven milestones are implemented. Each success criterion below is covered
by integration tests in `crates/newgit-core/tests/`.

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
- project the lane head into new workspaces
- capture, merge, pull, and checkout tracker content
- show tracker status, distinguishing never-pulled (`^`) from
  diverged-from-head (`~`) — direction is unknowable without rev ancestry,
  so status must not advise one

Success criterion:

> I can create a tracker, track a path, capture and merge it, and see later branch instances project that content.

### Milestone 3: Resources, Actions, Exports, And Ports

- parse resource definitions
- command-based resource actions
- minimal PID-file process supervision for `long_running` actions
- resource exports loaded into `newgit run`
- deterministic port allocation
- logs per action
- process resource template (pulled forward from Milestone 4)
- spawn runs `prepare` hooks in dependency order; failures are loud but
  leave the instance spawned; resources whose dependencies failed are marked
  `blocked` and are not prepared or started until the dependency is repaired

Success criterion:

> A resource can request a port, export it, and run a command that uses it.

### Milestone 4: Starter Definitions

- CLI-managed tracker creation and path tracking
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

- SQLite through a normal file tracker — no special support: a database that
  is just a file is a tracker, and binary content round-trips byte-for-byte
  through capture, checkpoint, and undo
- Postgres through the `command-snapshot` resource template depositing into a
  tracker; the template brings its `db-snapshots` lane with it, since a
  checkpoint whose `into_tracker` names a missing tracker fails at checkpoint
  time
- the `external` resource template, which needed `captures` to exist: newgit
  cannot compute a preview id, only ask for one and remember the answer
- resource `[cleanup]` hooks, so a resource newgit does not own can be torn
  down (see *Resource Cleanup Hooks*)

Success criterion:

> I can model a branch-local resource that newgit does not natively understand.

### Milestone 7: Basic Export

- path-level export honoring tracker audience, failing closed at `public`
- normal Git output: one commit, a branch, no remotes, no newgit refs
- no privacy claims, stated in the command's own output

Success criterion:

> A branch instance can produce a clean ordinary Git branch/repo as an output artifact.

### Milestone 8: Render

- `[[render]]` on resource definitions: literal `find`/`with` substitution
  into a committed path, run at bind before `prepare` and before a
  `recompute` restore
- the exactly-once match rule, `count = N`, and multi-line `find`; a
  zero-match or wrong-count render fails the resource and blocks dependents
- render reads the committed blob (`HEAD`, or the bound lane rev), never the
  working file
- simultaneous location and batch application, so declaration order is
  cosmetic; overlapping finds refused
- `--skip-worktree` for source-owned targets; export takes rendered paths
  from `HEAD`
- drift detection: the expected render recomputed and compared at checkpoint
  and before every re-render, naming the file and how much of it a re-render
  will discard
- reverse substitution on `newgit capture` for tracker-owned targets, with
  the rendered value's uniqueness checked at bind

Success criterion:

> A per-instance Supabase stack needs no config-rewriting code in its `prepare`
> hook, and the un-rendered repo still starts on its own defaults.

---

## What To Defer Until v2

Command shims:

- only after `newgit run` proves the exports and checkpoint machinery they reuse
- start with `git commit` (enrich) and `pnpm dev` / `npm run dev` (inject), the two highest-frequency reflexes
- `advise` shims for branch-creation commands next
- never emulate; every shim passes through to the real command and announces itself in output

Command environment policy:

- add `[run] env_files = [".env.local"]` and/or `newgit run --env-file .env.local`
- keep this outside trackers: trackers own file content; run policy decides which files become process environment
- this restores the useful `newgit run -- psql $DATABASE_URL` workflow without making tracker definitions export env

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
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit tracker create dev-db
newgit tracker track dev-db data/dev.sqlite
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
