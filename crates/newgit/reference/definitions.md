# newgit definition reference

Every key in a newgit definition file, what it accepts, and what it defaults
to. Printed by `newgit reference`, so it is on disk wherever the binary is.

`newgit reference` lists the sections; `newgit reference <name>` prints one
(`resource` brings its subsections with it); `newgit reference all` prints the
whole document.

The design narrative — why trackers and resources are the only two
primitives, what each one is for — lives in `newgit-v1-mvp.md` at
<https://github.com/Spheroman/newgit>. This page is the lookup table.

Definitions are TOML. The name of a tracker or resource comes from its
filename, never from a key inside the file.

Every key below is the complete list for its table: a key newgit does not
recognize is an error naming the file, the key, and what was expected, never
a line quietly skipped. A misspelled `ownership` would otherwise decide what
`newgit remove` may delete, and a misspelled `merge_with_source` would decide
whether a lane travels with a merge — both by defaulting, silently.

```
.newgit/
  config.toml            project settings          committed
  trackers/<name>.toml   tracker definitions       committed
  resources/<name>.toml  resource definitions      committed
  scripts/<name>         scripts hooks call        committed
  branches/ snapshots/ checkpoints/ logs/ state/ local/   local, gitignored
```

---

## Tracker — `.newgit/trackers/<name>.toml`

A named, versioned lane of file content. Create with `newgit tracker create`;
hand-edit afterwards.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `audience` | string | yes | — | who may read the lane. `public`, `project-devs`, `user`, or `user:<name>`. Only `public` ships in an `export` without an explicit `--include`. |
| `storage` | string | yes | — | `local` or `remote` — where synced state lives. v1 implements `local`. |
| `merge_with_source` | bool | no | `false` | whether a real source merge carries this tracker's bound state with it. |
| `paths` | array of strings | no | `[]` | workspace-relative paths the lane owns. Must not be absolute, contain `..`, or start with `.git`/`.newgit`. May be empty for a lane that only receives resource deposits (`into_tracker`). |

A tracker owns its paths outright: `newgit tracker track` adds them to the
store's `.gitignore`, and each workspace's Git is told to ignore them too, so
lane content never lands in source history.

`newgit tracker remove <name>` deletes the definition and reverses both of
those: the `.gitignore` block and each live workspace's ignore entries. It
refuses while any live instance still has the tracker bound (`newgit remove
<instance>` first) and refuses outright for `source`, the default tracker,
which Git/jj owns and has no definition file. Captured content under
`.newgit/snapshots/<name>/` is left in place — it becomes unreferenced, and
`newgit cleanup` reclaims it once nothing else (a checkpoint that captured
it) still pins a rev.

---

## Resource — `.newgit/resources/<name>.toml`

A lifecycle unit that re-establishes per-branch state that cannot travel as
file content. Start from `newgit resource add <name> --template <template>`
(`newgit resource templates` lists them; `newgit resource templates --show
<name>` prints one in full, including any companion resource or tracker it
creates alongside it, without instantiating anything).

`newgit resource remove <name>` deletes the definition. It refuses if another
resource still names it in `depends_on` — always, `--force` included, since
that would leave the graph pointing at nothing — and refuses if a live
instance still has it bound unless `--force` is given, in which case it drops
the binding from that instance's record and releases its ports (there is no
other ledger: a port is free the moment nothing claims it).

### Top level

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `ownership` | string | yes | — | `branch`, `workspace`, `project`, `user`, or `external`. See *Ownership*. |
| `depends_on` | array of strings | no | `[]` | resource or tracker names whose *lifecycle* this one depends on. Orders `prepare` at spawn and cleanup hooks in reverse. A name that is neither is an error the graph reports. Needing another resource's *value* is not this — see *Data edges*. |
| `workdir` | string | no | workspace root | where every command this resource runs is spawned, relative to the workspace root. Overridable per action — see `[actions.<name>]`. |

`workdir` is applied as the spawned process's working directory, never as a
`cd` prefix on the command string — so it cannot silently change what `&&`
binds, and a command that fails to `cd` can no longer look like it ran.
It governs one thing: where a *command* runs. It does **not** touch content
paths — `[identity].paths`, `[checkpoint].paths`, and `[[render]].path` are
always resolved against the workspace root, `workdir` or no. Mixing two
roots in one file, where some keys mean "relative to workdir" and others
mean "relative to the workspace," is exactly the confusion this is trying to
avoid — content paths keep the one meaning they have always had.

`[checkpoint]`, `[restore]`, and `[cleanup]` are not actions and have no
override of their own; their commands always run in the resource-level
`workdir`.

A `workdir` that does not exist when the command runs fails naming the
resource, the action, and the resolved path, rather than a bare shell error.
This is checked right before each command runs, not when the resource is
bound — a `workdir` may legitimately be created by an earlier action (a
`prepare` that clones a submodule into it, say), so it would be wrong to
require it to exist up front.

### `[identity]`

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `paths` | array of strings | yes if the section is present | — | the files whose content *is* this resource's identity (a lockfile, a manifest). Path-dependent state is recomputed from its identity, not copied. Workspace-relative, no `..`, and may not reach into `.git` or `.newgit`. |

This is the single declaration of what the resource is derived from, and the
other two keys read it rather than repeating it:

- `[checkpoint] mode = "hash"` records the content hash of these paths. It has
  no `paths` of its own — two places stating the same fact is how they come to
  disagree.
- `[restore] mode = "recompute"` compares that hash against the workspace as it
  was before the undo, and **skips the rebuild when they match**. Same inputs,
  same tree; re-running would be an expensive no-op.

Installs are the usual reason for this section, and they are the one place
newgit multiplies a cost rather than absorbing it — see *Installs: use a
content-addressed store*.

### `[ports.<name>]`

One entry per port the resource needs. `<name>` is yours (`app`, `db`).

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `start` | integer | yes | — | where to start scanning. The allocated port is the first one from `start` upward that is neither promised to another instance nor unbindable right now. |
| `env` | string | no | none | environment variable the port is published as to `newgit run` and actions (e.g. `PORT`). |

A port is allocated once, at `spawn`, and recorded in the binding record. It
never changes for the life of the instance; removing the instance frees it.
Use it in templates as `{{ports.<name>}}`.

### `[actions.<name>]`

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `command` | string | yes, unless `signal` is set | — | shell command, run in the workspace (or `workdir`, below). |
| `workdir` | string | no | the resource's `workdir` | replaces — does not nest under — the resource-level `workdir` for this action only. |
| `long_running` | bool | no | `false` | supervise it: `newgit action` returns once started, output goes to a log, and the process group is tracked. Requires `command`. |
| `signal` | string | no | `term` for a `stop` action | makes an action signal-only. An action with `signal` and no `command` stops this resource's supervised process. |
| `captures` | array of strings | no | `[]` | names to read out of the command's stdout and publish as this resource's exports. See *Captures*. |

**Actions are not lifecycle hooks.** Only `prepare` runs on its own — at
`spawn`, and again for a `recompute` restore. Every other action is something
you invoke: `newgit action <resource>.<action> [instance]`. Name them whatever
you like; `start`/`stop` are a convention, made real by `long_running` and
`signal`, not by the names.

### `[checkpoint]`

What this resource records when `newgit checkpoint` runs.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `mode` | string | yes | — | `none`, `hash`, `command`, or `external`. `hash` requires `[identity]`, whose paths it hashes. |
| `command` | string | required for `command` | — | emits the state. Its trimmed stdout is the state ref. |
| `into_tracker` | string | no | none | `command` only: deposit what the command wrote under `{{snapshot.path}}` into this tracker's lane, recording the state ref as `tracker:<name>@<rev>`. This is the one seam between resources and trackers. |
| `state_ref` | string | required for `external` | — | template for the opaque handle to record (usually `{{exports.<name>}}`). |

| mode | what a checkpoint stores | typical use |
| --- | --- | --- |
| `none` | nothing | a resource whose state is uninteresting |
| `hash` | content hash of `[identity] paths` | installs — the lockfile is the truth |
| `command` | the command's stdout, plus optionally a lane deposit | a database dump |
| `external` | a rendered handle | a cloud preview, a tunnel |

### `[restore]`

What `newgit undo` does with that record.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `mode` | string | yes | — | `none`, `command`, `recompute`, or `external`. |
| `command` | string | required for `command` | — | re-establishes the state; `{{state_ref}}` is the checkpointed handle, or the path of a deposited snapshot. |
| `action` | string | no | `prepare` | `recompute` only: the action to re-run. Must exist. |

| mode | what an undo does | typical use |
| --- | --- | --- |
| `none` | nothing | state that does not need rewinding |
| `command` | runs `command` | restore a dump |
| `recompute` | re-runs an action, unless `[identity]` has not moved | reinstall from the restored lockfile |
| `external` | nothing, deliberately | another system owns it; an undo does not rewind it |

**A checkpoint and a restore have to agree.** They are two halves of one
mechanism — one records a state ref, the other consumes it — so what cannot
mean anything is refused when the definition loads, not during the undo you
are relying on:

| this | is refused because |
| --- | --- |
| `{{state_ref}}` in a `[restore]` or `[cleanup]` command under `mode = "hash"` | a content hash identifies *inputs*. `restore-from hash:0fa284b468` is not a no-op, it is a wrong argument. Rebuild from those inputs with `recompute`, or drop the placeholder. |
| the same under `mode = "none"`, or with no `[checkpoint]` at all | nothing records a ref, so the placeholder can never resolve. |
| `external` + `recompute` | `recompute` does not ignore the handle, it re-runs `prepare` — which for an external resource mints a *second* instance and orphans the one the handle names. Use `command`, which receives it, or `external`. |

The first two rules are about the placeholder, not the mode: a restore command
that never asks for a state ref is an ordinary rebuild whatever the checkpoint
records, and it loads. So do pairings that are merely inert — a `command`
checkpoint with a `recompute` restore ignores the ref it recorded, but the
record still reads back in `newgit checkpoints`.

A `[cleanup]` hook is also refused *at teardown* if its `{{state_ref}}` has no
value — see `[cleanup]`. The load-time rules above cannot catch everything the
runtime one does: a checkpoint record written under an older definition is not
something reading the current file can predict.

A `recompute` restore skips when this resource's `[identity]` hash is the same
now as at the checkpoint: the tree was already built from those inputs, so the
rebuild would change nothing. It says so rather than passing silently:

```
resource: deps recompute(prepare) skipped: identity unchanged
```

Identity describes the *inputs*, not the tree. Delete half of `node_modules`
without touching the lockfile and the hash still matches while the tree is
wrong — `newgit undo --force-recompute` rebuilds anyway.

`newgit undo --only <resource>` restores one resource and leaves source,
tracker content, and every other resource untouched. That is not a snapshot
the instance was ever in, so it is reported as a partial restore and never as
"restored to <checkpoint>". It exists for iterating on a restore command,
where rewinding the whole workspace each cycle is the cost.

A restore command is not transactional. If one fails, `newgit undo` says the
undo was incomplete and names the resource, rather than reporting success.

### `[cleanup]`

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `command` | string | no | none | tears the concrete resource down. Runs at `newgit remove`, and at `newgit cleanup` for an instance whose workspace is gone, dependents before dependencies. A `[cleanup]` section without one means there is nothing to do, and teardown says so. |

Two rules constrain it, both deliberately:

- **Ownership decides whether it runs at all.** A `project`- or `user`-owned
  resource is shared beyond the instance, so per-branch teardown skips its
  hook even when one is defined — and says it skipped it.
- **An unresolved `{{...}}` refuses.** Everywhere else a placeholder with no
  value renders verbatim so the mistake is visible. A destructive command is
  the exception: `delete {{state_ref}}` with no state ref is not run. A `hash`
  checkpoint counts as no state ref: it records the content hash of
  `[identity] paths`, which says whether the inputs moved and never names a
  concrete thing to tear down, so `{{state_ref}}` stays unresolved and the
  hook is refused rather than handed `hash:0fa284b468`.

### `[exports]`

`NAME = "value"` pairs, rendered once at `spawn` and stored in the binding
record. They become environment variables for `newgit run` and for this
resource's actions, and are readable in later hooks as `{{exports.NAME}}`.

```toml
[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
```

An export may compose a dependency's export, the same way a `[[render]]` can —
bindings happen in dependency order, so everything upstream is already
resolved:

```toml
depends_on = ["supabase"]

[exports]
SUPABASE_FUNCTIONS_URL = "{{exports.SUPABASE_API_URL}}/functions/v1"
```

It may compose its own siblings too, in any order. The table is a map, not a
sequence, so `HEALTH_URL` below is rendered before `BASE_URL` exists on the
first pass and resolved on the second — you never have to think about which
line comes first:

```toml
[exports]
BASE_URL   = "http://127.0.0.1:{{ports.app}}"
HEALTH_URL = "{{exports.BASE_URL}}/health"
```

Two exports that reference each other resolve to nothing and are reported
like any other placeholder that never resolved.

**An unresolved `{{...}}` refuses.** This is the third place that does, with
`[cleanup]` and `[[render]]`, and for the same reason: everywhere else a
placeholder with no value renders verbatim so the mistake is visible to
whoever typed it, but an export is rendered once, written to the binding
record, and handed to every later action and `newgit run` as an environment
variable. The mistake would surface in a different process, hours later, as a
malformed URL. The resource fails to bind and the value is not stored — an
absent variable is something downstream can detect; `http://127.0.0.1:{{ports.db.api}}`
is not.

It withholds *every* export of that resource, not just the bad one, and names
every export that failed rather than the first. A binding that publishes half
an environment is the same failure one variable further down: `newgit run` and
every dependent's actions read a binding's exports without asking what status
it holds.

There is no syntax for another resource's *ports*: `{{ports.<name>}}` is
scoped to the resource's own. Publish the value as an export and compose that.

Export names are global to the project and may only be claimed once — see
*Command environment*. Two resources that both want `APP_URL` must pick two
names; that is the same constraint the shell they end up in has.

### Data edges

`{{exports.<name>}}` in an `[exports]` value or a `[[render]]` replacement is
itself the declaration that you need another resource's value. newgit reads
the edge out of the template; you do not write it in `depends_on`:

```toml
# supabase.toml — no depends_on. The template already said it.
[[render]]
path = "packages/db/supabase/config.toml"
replace = [
  { find = 'additional_redirect_urls = ["exp://127.0.0.1:8081"]',
    with = 'additional_redirect_urls = ["{{exports.EXPO_URL}}"]' },
]
```

`web` exports `EXPO_URL`, so `web` binds before `supabase` and the value is
there when the file renders. `newgit resource list` prints what was inferred,
since an edge nobody wrote down still has to be legible:

```
Reads exports from (inferred from `{{exports.*}}`):
  supabase reads web (EXPO_URL)
  these order binding only — they say nothing about teardown
```

**A data edge orders binding and nothing else.** It does not claim that
`supabase` needs `web` running, started, or ever used, and teardown ignores
it completely — `[cleanup]` hooks reverse the `depends_on` graph alone. That
separation is the point: needing one string out of a resource used to require
declaring a lifecycle dependency that did not exist, which then quietly
reversed into teardown order.

Two rules follow from reading the edge rather than being told it:

- A name no resource exports is not an edge. It is a template that will not
  resolve, and `[exports]` and `[[render]]` already refuse it at bind time
  with a better message than a graph error could give.
- A resource referring to *its own* exports is the sibling case above, not an
  edge to itself.

`[cleanup]` and `[checkpoint]` may use `{{exports.*}}` too, but they read a
binding record that is already complete, so they create no edge — there is
nothing left to order.

A cycle through data edges alone (`A = "{{exports.B}}"` in one resource,
`B = "{{exports.A}}"` in another) is a real cycle and is reported like any
other: the two cannot be bound in either order.

Resources export runtime values — ports, URLs, handles. Trackers do not
export anything; they own file content.

### `[[render]]`

Substitutes this instance's values into a file the project commits — for the
common case where a tool reads its port from a config file rather than argv.
Repeatable: one `[[render]]` per file.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `path` | string | yes | — | workspace-relative path to the file. Must be tracked by Git or owned by a tracker: a render substitutes into committed content, so there has to be some. Two resources rendering one path is refused at load. |
| `replace` | array of tables | yes | — | the substitutions, below. |

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `find` | string | yes | — | **a literal string, never a regex.** May span lines. |
| `with` | string | yes | — | what to put there; rendered with the variables below. |
| `count` | integer | no | `1` | how many times `find` is expected to occur. |

```toml
[[render]]
path = "supabase/config.toml"
replace = [
  { find = 'project_id = "faretable"', with = 'project_id = "faretable-{{branch.slug}}"' },
  { find = "port = 54321",             with = "port = {{ports.api}}" },
]
```

**There is no template file.** `port = 54321` is not a placeholder — it is
your project's working default, so a clone without newgit still starts on it.
newgit substitutes into the committed content and writes the result into one
workspace.

Four rules carry the rest:

- **A `find` must occur exactly `count` times, or the render refuses**, naming
  the file and the string. That is also the drift detector: when the default
  changes upstream, you hear about it at `spawn` instead of getting a file
  that quietly went unrendered. Where a value is genuinely repeated, declare
  `count = 2` rather than reaching for an "all" flag — a declared number keeps
  failing when the file changes from two occurrences to three. Where two
  sections share a default, make `find` multi-line.
- **Replacements are simultaneous.** Every `find` is located in the committed
  content and the whole batch applies at once, so a replacement's output is
  never a match target and reordering `replace` cannot change the result. Two
  rules claiming overlapping text are refused.
- **Committed content is the input, never the working file** — `HEAD`, or the
  bound lane rev for a tracker-owned path. So a render is idempotent: `undo`,
  `tracker pull`, and `tracker checkout` re-render off the binding record, and
  values never compound.
- **Your values stay in this workspace.** A rendered source path is marked
  `--skip-worktree`, so it never shows in `git status` and `git add -A` cannot
  commit it; `newgit export` and a checkpoint's uncommitted-state capture take
  it from `HEAD`; and for a tracker-owned path `newgit tracker capture`
  reverses the substitution, so a key you add to `.env.local` reaches the lane
  and your port does not.

The cost, on a source path: **hand edits to a rendered file do not survive the
workspace.** newgit does not restate that every spawn — it recomputes what the
render should produce and compares, at `checkpoint` and before each re-render,
naming the file and how many lines are about to go. Silence means there is
nothing to lose. To change a rendered file for real, change it in the store
repo.

---

## Ownership

Who owns the concrete instance, and what teardown may touch. Operational, not
a security label.

| value | means | per-branch cleanup runs its hook? |
| --- | --- | --- |
| `branch` | one per branch instance (a database named after the branch) | yes |
| `workspace` | one per workspace (`node_modules` in the tree) | yes |
| `project` | shared by every instance of this project | no |
| `user` | shared beyond this project (a pnpm store) | no |
| `external` | another system owns it; newgit holds a handle | yes — exactly the `[cleanup] command`, nothing else |

---

## Installs: use a content-addressed store

> **Strongly recommended: choose a package manager that installs from a shared
> content-addressed store.** It is the single choice that most affects what
> running many instances costs you, and it is not one newgit can make for you.

Every branch instance installs its own dependencies. That is not a default to
turn off and it is not newgit being wasteful: two branches with different
lockfiles must not share a dependency tree, or one branch's install silently
rewrites the other's. It is why installs are resources with `[identity]` and a
`recompute` restore rather than trackers — the lockfile is the truth, and the
tree is rebuilt from it.

What that independence costs is set by your package manager, not by newgit:

| tool | per-instance cost | why |
| --- | --- | --- |
| pnpm | directory entries | hardlinks packages from one global store |
| Yarn PnP | ~nothing | no install tree at all |
| uv, bun | directory entries | hardlink from a shared cache |
| Cargo | a `target/` each | the registry is shared; build output is not |
| npm, Yarn classic | a **full copy** each | the cache holds tarballs, so `npm ci` re-expands every time |
| pip into a venv | a **full copy** each | same shape |

Ten instances of a monorepo is roughly one `node_modules` worth of disk under
pnpm and ten under npm. The difference is not a tuning detail; it is whether
keeping eight branches alive at once feels free or feels like something you
ration.

There is no lever on newgit's side, because the thing that would save the
space — one installed tree shared between instances — is exactly the bug this
design exists to prevent. So if you are adopting newgit and have a choice,
make it here first.

Two keys matter when you wire the store up:

- Give the shared store its own resource with **`ownership = "user"`**, and
  have the install `depends_on` it. `user` is the one ownership newgit never
  deletes, at `remove` or at `cleanup` — correct, because that store is shared
  with every other project on the machine. The `pnpm` template ships this pair
  already.
- Point the install's **`[identity] paths`** at the lockfile *and* the
  manifest, so a `recompute` restore reinstalls exactly what the checkpoint
  described.

If you cannot switch package managers, nothing breaks — it costs disk. Run
fewer concurrent instances, and let `newgit cleanup` reclaim the trees of
instances whose workspaces are gone.

---

## Template variables

Every command and export value is rendered before it runs. An unknown or
out-of-scope variable is left in the text verbatim — visible rather than
silently empty — except in `[cleanup]`, `[[render]]`, and `[exports]`, which
refuse instead. All three write something durable: a destructive command, a
file that would otherwise gain committed-looking text nobody wrote, and an
environment variable handed to every later process.

| variable | is |
| --- | --- |
| `{{branch.name}}` | the instance name as you typed it |
| `{{branch.slug}}` | its filesystem-safe form, and the workspace directory name |
| `{{workspace}}` | absolute path to the workspace |
| `{{scripts}}` | absolute path to `.newgit/scripts/` in the store — use `{{scripts}}/<name>` so a hook's script is read from the store, like the definition calling it |
| `{{ports.<name>}}` | an allocated port |
| `{{exports.<name>}}` | a rendered export from this resource's binding |
| `{{snapshot.path}}` | directory to write checkpoint output into |
| `{{state_ref}}` | the handle the last checkpoint recorded, or the path of the snapshot it deposited. A `hash` checkpoint records neither, so it has no value |

Scope — which of them have a value where:

| rendered in | branch/workspace/scripts | `ports.*` | `exports.*` | `snapshot.path` | `state_ref` |
| --- | --- | --- | --- | --- | --- |
| `[exports]` values | yes | yes | yes | — | — |
| `[[render]]` `with` | yes | yes | yes | — | — |
| action `command` | yes | yes | — | — | — |
| `[checkpoint] command` | yes | yes | yes | yes | — |
| `[checkpoint] state_ref` | yes | yes | yes | — | — |
| `[restore] command` | yes | yes | yes | — | yes |
| `[cleanup] command` | yes | yes | yes | — | yes |

Action commands do not get `{{exports.*}}`: exports reach them as environment
variables, which is what a command already knows how to read. A `[[render]]`
does get them, because a file is not a process — nothing hands it an
environment. It sees what a command in this instance would see: its own ports,
plus every export bound so far in dependency order, which is what lets one
resource's config file carry another's URL.

---

## Captures

An action with `captures` publishes values its command printed as this
resource's exports — how a resource that mints an external handle (a preview
id, a tunnel URL) hands it to everything downstream. The `external` template
(`newgit resource templates --show external`) is built around this: its
`prepare` action captures `PREVIEW_ID` and `PREVIEW_URL` from a provisioning
command's JSON output.

Two stdout shapes are accepted, because they are what real commands already
emit:

- stdout whose first non-whitespace character is `{` is parsed as a **flat
  JSON object**; scalars are stringified, containers are skipped;
- anything else is read as **`KEY=VALUE` lines**.

Only declared names are taken. **When `captures` is set, stdout belongs to
newgit** — send progress output to stderr, or a chatty CLI's noise is
interleaved with the values and they will not parse. A declared name the
command never emitted is a warning, not an error.

---

## Command environment

`newgit run [instance] -- <command>` and every action see:

1. each resource's rendered `[exports]`, in dependency order;
2. `[ports.<name>] env` variables;
3. `NEWGIT_BRANCH` and `NEWGIT_WORKSPACE`.

Trackers contribute no environment; they place files. Loading `.env`-style
files is command-run policy, not a tracker feature.

**A name has exactly one owner.** Three things declare an environment
variable — an `[exports]` key, an action's `captures` entry, and a port's
`env` — and the same name appearing in two of them is a graph problem,
reported by `newgit resource list` and refused by `spawn`, `run`, `action`,
`checkpoint`, and `undo`, like a missing dependency or a cycle:

```
environment variable `EXPO_URL` is declared more than once (`metro` [exports],
`supabase` [exports]); a name may have only one owner — rename all but one,
and compose it elsewhere with `{{exports.EXPO_URL}}`
```

There is no shadowing rule to learn because there is nothing to shadow. The
one deliberate overlap is a `captures` entry naming its *own* resource's
`[exports]` key: the export holds the value the definition can state up
front, and the action overwrites it with the one that did not exist until it
ran. The owner is the same resource either way.

`NEWGIT_BRANCH` and `NEWGIT_WORKSPACE` are reserved. newgit sets them for
every command it runs, so a resource declaring one is reported too — a
declaration that could never reach the process is a mistake worth naming,
not a silent no-op.

This is why the starter templates do not ship conventional names like `PORT`
or `DATABASE_URL`. Two services in one project cannot both publish `PORT`,
so `newgit resource add web --template process` writes `WEB_PORT` and
`WEB_URL`, naming them after the resource. Rename them if you have one
service and your tool insists on `PORT` — the generated file says so.

---

## Project — `.newgit/config.toml`

Written by `newgit init`.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `project.name` | string | yes | the repository directory name | used in the default workspace root. |
| `project.source` | string | yes | detected | `git` or `jj` (a colocated `.git` is required in v1). |
| `workspace.root` | string | no | `~/.newgit/workspaces/<name>-<hash>/` | where workspaces are materialized. `~` expands. Omitted from the generated file so a committed config never bakes in one user's absolute paths. |
| `workspace.materializer` | string | no | real directories | reserved; v1 materializes full clones. |
