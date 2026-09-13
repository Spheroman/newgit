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

---

## Resource — `.newgit/resources/<name>.toml`

A lifecycle unit that re-establishes per-branch state that cannot travel as
file content. Start from `newgit resource add <name> --template <template>`
(`newgit resource templates` lists them).

### Top level

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `kind` | string | yes | — | a label for you, **not** a behavior switch. newgit never branches on it; what a resource does comes from the sections below. The templates use `process`, `command`, `command-snapshot`, `external`, `external-store`. |
| `ownership` | string | yes | — | `branch`, `workspace`, `project`, `user`, or `external`. See *Ownership*. |
| `depends_on` | array of strings | no | `[]` | resource or tracker names that must be ready first. Orders `prepare` at spawn and cleanup hooks in reverse. A name that is neither is an error the graph reports. |

### `[identity]`

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `paths` | array of strings | yes if the section is present | — | the files whose content *is* this resource's identity (a lockfile, a manifest). Path-dependent state is recomputed from its identity, not copied. |

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
| `command` | string | yes, unless `signal` is set | — | shell command, run in the workspace. |
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
| `mode` | string | yes | — | `none`, `hash`, `command`, or `external`. |
| `paths` | array of strings | required for `hash` | `[]` | identity files whose content hash is the captured state. |
| `command` | string | required for `command` | — | emits the state. Its trimmed stdout is the state ref. |
| `into_tracker` | string | no | none | `command` only: deposit what the command wrote under `{{snapshot.path}}` into this tracker's lane, recording the state ref as `tracker:<name>@<rev>`. This is the one seam between resources and trackers. |
| `state_ref` | string | required for `external` | — | template for the opaque handle to record (usually `{{exports.<name>}}`). |

| mode | what a checkpoint stores | typical use |
| --- | --- | --- |
| `none` | nothing | a resource whose state is uninteresting |
| `hash` | content hash of `paths` | installs — the lockfile is the truth |
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
| `recompute` | re-runs an action | reinstall from the restored lockfile |
| `external` | nothing, deliberately | another system owns it; an undo does not rewind it |

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
  the exception: `delete {{state_ref}}` with no state ref is not run.

### `[exports]`

`NAME = "value"` pairs, rendered once at `spawn` and stored in the binding
record. They become environment variables for `newgit run` and for this
resource's actions, and are readable in later hooks as `{{exports.NAME}}`.

```toml
[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
```

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
silently empty — except in `[cleanup]` and `[[render]]`, which refuse instead.
Both write something durable: a destructive command, and a file that would
otherwise gain committed-looking text nobody wrote.

| variable | is |
| --- | --- |
| `{{branch.name}}` | the instance name as you typed it |
| `{{branch.slug}}` | its filesystem-safe form, and the workspace directory name |
| `{{workspace}}` | absolute path to the workspace |
| `{{scripts}}` | absolute path to `.newgit/scripts/` in the store — use `{{scripts}}/<name>` so a hook's script is read from the store, like the definition calling it |
| `{{ports.<name>}}` | an allocated port |
| `{{exports.<name>}}` | a rendered export from this resource's binding |
| `{{snapshot.path}}` | directory to write checkpoint output into |
| `{{state_ref}}` | the handle the last checkpoint recorded, or the path of the snapshot it deposited |

Scope — which of them have a value where:

| rendered in | branch/workspace/scripts | `ports.*` | `exports.*` | `snapshot.path` | `state_ref` |
| --- | --- | --- | --- | --- | --- |
| `[exports]` values | yes | yes | — | — | — |
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
id, a tunnel URL) hands it to everything downstream.

```toml
[actions.prepare]
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["PREVIEW_ID", "PREVIEW_URL"]
```

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

`newgit run [instance] -- <command>` and every action see, in this order
(later wins):

1. each resource's rendered `[exports]`, in dependency order;
2. `[ports.<name>] env` variables;
3. `NEWGIT_BRANCH` and `NEWGIT_WORKSPACE`.

Trackers contribute no environment; they place files. Loading `.env`-style
files is command-run policy, not a tracker feature.

---

## Project — `.newgit/config.toml`

Written by `newgit init`.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `project.name` | string | yes | the repository directory name | used in the default workspace root. |
| `project.source` | string | yes | detected | `git` or `jj` (a colocated `.git` is required in v1). |
| `workspace.root` | string | no | `~/.newgit/workspaces/<name>-<hash>/` | where workspaces are materialized. `~` expands. Omitted from the generated file so a committed config never bakes in one user's absolute paths. |
| `workspace.materializer` | string | no | real directories | reserved; v1 materializes full clones. |
