# newgit definition reference

Every key in a newgit definition file, what it accepts, and what it defaults
to. Printed by `newgit reference`, so it is on disk wherever the binary is.

`newgit reference` lists the sections; `newgit reference <name>` prints one
(`resource` brings its subsections with it); `newgit reference all` prints the
whole document.

An agent setting newgit up for someone: read `newgit reference agents` first.

The design narrative — why trackers and resources are the only two
primitives, what each one is for — lives in `docs/design/spec.md` at
<https://github.com/Spheroman/newgit>. This page is the lookup table.

Definitions are TOML. The name of a tracker, resource, or export target
comes from its filename, never from a key inside the file.

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
  exports/<name>.toml    export targets            committed
  scripts/<name>         scripts hooks call        committed
  branches/ checkpoints/ logs/ state/ local/   local, gitignored
  local/trackers.git     every tracker's history   local, gitignored
  installs/              built trees, shared       local, gitignored
```

---

## Agents — setting newgit up for someone

You are probably here because someone installed newgit and asked you to set
it up. Assume they know nothing about it beyond the name. They are relying on
you to find out what newgit can do for *their* project and to show them.

**`newgit init` is not the setup.** On its own it leaves the three built-in
trackers and no resources. Every spawn then gets the code and nothing else:
no `.env.local`, no installed dependencies, no dev server, no database. That
is `git worktree` with extra steps, and anyone who only sees that will
reasonably decide newgit isn't worth keeping. A useful setup gives every
instance what the project needs to run. Spawn should give them a branch they
can run right away, and undo should put back the env and data as well as the
code.

### Look before you ask

Read the project before you propose anything. Most of what a setup needs is
already visible:

```sh
git status --short --ignored        # ignored files the project needs: tracker candidates
ls -a; cat package.json             # or pyproject.toml, Cargo.toml, Makefile, …
newgit resource templates           # what each template is for
```

| you find | it becomes |
| --- | --- |
| `.env`, `.env.local`, `.envrc`, `config/*.local.*`, a credentials file that is gitignored | a secret tracker: `audience = "user"` if each person has their own, `project-devs` if the team shares one |
| a lockfile (`pnpm-lock.yaml`, `package-lock.json`, `uv.lock`, `Cargo.lock`, …) | an install resource: `--template pnpm` for pnpm, `--template install` for anything else |
| a `dev` / `start` script, a server entry point | `--template process`: its own port, started for each instance |
| `supabase/` | `--template supabase`: its own stack and ports per instance, with snapshots on checkpoint |
| Postgres/MySQL in Docker or Compose, migrations | `--template command-snapshot` or `command-snapshot-migrations` |
| two env files for one thing (`.env.staging`, `.env.local-db`) | two trackers sharing a `role`, so each spawn picks one (`--use`) |
| an app or package in a monorepo that only some branches touch | a code tracker (`--code --path apps/x`), dropped from a spawn with `--drop` |
| local agent notes or settings (`CLAUDE.local.md`, scratch plans) | a `user` tracker, so the notes go with each branch and come back on undo |
| a hard-coded port in a committed config file | a `[[render]]` on that resource (`newgit reference render`) |
| a GitHub remote and `gh` in use | nothing to add. `git push` and `gh pr create` work from a workspace |

### Be creative

The table above is a starting point, not the whole menu. newgit is a new kind
of tool, and neither you nor the user will have habits for most of what it
can do. Don't stop at copying a setup you'd build with worktrees and a
`.env` file. **Shape the tools to the project, not the project to a
preconceived notion of what the tool can do.** Trackers, roles, priorities,
audiences, and resources combine freely. Look for where this project hurts
and build the setup around that. Some patterns:

- **A shared database by default, a branch database when you need one.** If
  the database stack is slow or expensive to spin up, giving every branch its
  own wastes most of that. Make two trackers that fill the `db` role. The
  default points every instance at one shared database. The other brings a
  per-branch stack, for schema changes, migrations, or tests that need
  isolation:

  ```toml
  # .newgit/trackers/shared-db.toml — what a spawn gets unless asked
  audience = "project-devs"
  storage = "local"
  role = "db"
  priority = 10
  paths = [".env.db"]              # points at the one shared database

  # .newgit/trackers/branch-db.toml — `--use branch-db`, or swap in later
  audience = "project-devs"
  storage = "local"
  role = "db"
  paths = [".env.db"]              # points at 127.0.0.1:54322
  resources = ["supabase"]         # a stack of its own, started only here
  ```

  Both trackers own `.env.db`, which is allowed because they share a role and
  are never bound together. The branch copy names a default port, and a
  `[[render]]` in the `supabase` resource replaces it with the instance's own
  (`{{ports.db}}`), so each branch's `.env.db` points at its own stack.

  A branch that turns out to need its own database runs `newgit tracker use
  branch-db`. That swaps the role over and starts the stack, with no respawn.
- **Live keys you only get when you ask.** Put test and live API keys in two
  trackers sharing a role, and give the live one a negative `priority`. Then
  no spawn, and so no unattended agent, binds live credentials unless someone
  passes `--use`.
- **A monorepo where each branch carries only what it touches.** Make each app
  a code tracker with a negative `priority`. A spawn checks out only the apps
  it binds. An install that builds `{{bound.paths}}` installs only those, and
  the install store keys on the binding, so branches working on the same app
  share one tree.
- **Agent notes that move with the branch.** A `user` tracker over
  `CLAUDE.local.md` or a `notes/` directory keeps an agent's plans and
  scratch work with its branch. They're checkpointed and undone with the
  code, and never show up in a diff.
- **Data you can rewind.** A database resource that snapshots on checkpoint
  makes `undo` restore the data along with the code. That makes it safe to
  let an agent run a destructive migration.
- **Anything with a handle.** The `external` template wraps whatever another
  system owns, like a preview deploy, a tunnel, or a cloud sandbox. Its URL
  becomes an export every other resource can read, and `remove` tears it down.
- **Different people, different views.** Audiences go down to `user:<login>`.
  The ops credentials one person holds can be a tracker only they ever
  receive, inside a project everyone else works on too.

Check each idea against the reference before you propose it, and run it
through *Prove it works* like everything else. If the project needs something
newgit can't express, say so plainly rather than bending the project to fit.
Proposing one of these is also the best way to show a new user why newgit is
worth keeping.

### Explain it, then ask

**We highly recommend explaining what newgit does before you ask the user
anything.** Someone who has never heard of a tracker can't tell you which
trackers they want. Asked cold, they will either guess or say "whatever you
think", and you lose the one input only they can give. Ask them before
they understand newgit and the questions read as busywork.

Start with what newgit is, in a few sentences. Each branch becomes an
*instance*: its own workspace with everything it needs to run. *Trackers*
hold the files Git shouldn't, like env files and local config. Each has its
own history and is never committed. *Resources* are what each branch has to
rebuild or run, like installs, dev servers and databases. A *checkpoint*
saves all of it, and `undo` puts it all back. Use these words. The user will
see them in every command, so this is when to learn them.

Then tell them what you found and what each piece gets them. Keep it short.
For example:

> newgit gives each branch its own copy of the project, ready to run. Files
> Git shouldn't hold go in *trackers*, and the things each branch has to
> install or run are *resources*. For this project I'd set up:
> - a tracker for your `.env.local`, copied into every branch and versioned
>   but never committed, so a branch an agent broke can be undone env and
>   all;
> - `pnpm install`, built once per lockfile and shared, so a new branch
>   doesn't reinstall;
> - `pnpm dev` on its own port in each branch, so five can run side by side;
> - your Supabase database, one per branch, snapshotted on checkpoint.
>
> Should `.env.local` be per-person, or shared with your team? Do you also
> point at a staging database sometimes? I can make that a second env you
> pick at spawn time.

Only then **ask about trackers**, because those are the ones you can't
infer:

- Which ignored files does a branch need in order to run? You will have found
  some of them. Ask whether there are others, such as files that live outside
  the repository today and get copied in by hand.
- Who should see each one: just them (`user`), the team (`project-devs`), or
  everyone (`public`, which ships in exports)? Default to `user` for anything
  with credentials in it.
- Are there alternatives they switch between, like a local and a staging
  database or a test and a live API key? Those become trackers sharing a
  `role`.
- Is there anything they would want undo to bring back that Git doesn't
  hold?

If they don't care to choose, set up what you found with the safe defaults and
tell them what you picked.

### Build it

```sh
newgit init
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit tracker capture runtime-env --from-store   # seed it from the file that's already there
newgit resource add deps --template pnpm
newgit resource add app --template process
```

Then open every generated file in `.newgit/resources/` and fix the lines
marked `EDIT ME`, such as the dev command and the lockfile. A template's
defaults are guesses, and a wrong guess shows up as a failed spawn later. `newgit
reference <section>` has every key. If a resource needs a `[[render]]`, run
`newgit render --check`.

Make sure an install resource declares `[identity] produces`, the directory
the install writes (`node_modules`, `.venv`). That is what lets later
instances with the same lockfile be cloned from the install store instead of
installing again. The `pnpm` template sets it; the `install` template leaves
it as `EDIT ME`, and an install without it is never shared.

Commit `.newgit/` the way `init` says to, on a branch if the project works
that way. A spawn reads definitions from the store, but teammates and CI only
see what is committed.

### Prove it works

Don't hand it over until a real spawn comes up:

```sh
newgit spawn newgit-try
newgit status newgit-try          # every resource ready; if not, it says why
newgit env newgit-try             # ports and exports, labeled by where they came from
newgit run newgit-try -- <the project's test or health check>
newgit checkpoint newgit-try --verify
newgit remove newgit-try
```

Fix what fails and run it again. A setup that fails on its first spawn is
worse than not setting it up at all.

### Wire up the tools they already use

- **Claude Code:** `newgit claude-code install`. Then every `claude
  --worktree`, background session, and worktree-isolated subagent gets a full
  instance instead of a bare worktree.
- **T3 Code:** the `Spheroman/t3code` fork turns each thread into an instance
  and stops its resources when the thread is settled. Mention it if they use
  T3 Code.
- **A newgit remote** (`newgit login`, `newgit remote add`) lets checkpoints
  travel, so a teammate's or cloud agent's branch comes up locally with its
  uncommitted work. The hosted one is invite-only, so offer it but don't
  assume they have access.

### Show them

Finish with a demonstration rather than just a summary. Spawn two instances
side by side and show that they run on different ports at once. Checkpoint
one, break something in it, and `undo` it. Then leave them a short list of
the commands they will use every day: `spawn`, `status`, `checkpoint`,
`undo`, `remove`. Also say what you set up and what you chose not to.

### Guardrails

- Secret by default is the point. Never move tracker content into Git, never
  give a credentials file a `public` audience, and never widen an audience
  without asking.
- Ask before anything that leaves the machine: `newgit remote add`, `members
  add`, `export --remote`.
- newgit does not rewrite history, so don't work around it by doing so
  yourself.
- Be honest about the limits. newgit needs git 2.38+ and a Unix-like OS.
  An install whose lockfile no other instance has built runs the package
  manager for real: npm and pip copy a full tree each time, while pnpm, uv,
  and bun mostly hardlink. Later instances with the same lockfile are cloned
  from the install store (`newgit reference installs`).

---

## Tracker — `.newgit/trackers/<name>.toml`

A named, versioned history of file content. Create with `newgit tracker
create`; hand-edit afterwards.

Three trackers always exist: `root` owns `.` — every path no more specific
tracker claims — and its history starts as the project's Git history;
`project` owns `.newgit` (resources, scripts, config); and `access` owns
`.newgit/trackers/`, every tracker's definition, with `maintainers =
"owner"`. Ownership nests: the most specific claim wins, so
`root` owning `.` and `runtime-env` owning `.env.local` do not conflict. Two
trackers claiming the identical path is an error.

A tracker is *secret* (the default) or *code*. Secret content is captured
into the tracker's history and never committed to Git. Code content
(`secret = false`) is ordinary committed source with its own history — `root`,
`project`, and anything made with `newgit tracker create <name> --code --path
<path>`, which carves those paths, history included, out of whichever tracker
owned them. You commit to a code tracker with Git; `newgit tracker capture`
and `track` refuse it.

```toml
# .newgit/trackers/runtime-env.toml — secret: each user's own .env.local
audience = "user"
storage = "local"
paths = [".env.local"]

# .newgit/trackers/staging-db.toml — one of two trackers filling a role
audience = "project-devs"
storage = "remote"
role = "db"
priority = 10
paths = [".env.db"]
resources = ["staging-tunnel"]
```

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `audience` | string | yes | — | who may read it. `public`, `project-devs`, `user` (each user gets their own copy), or `user:<login>`. Only `public` ships in an `export --to` without an explicit `--include`, or to an export target that does not name it. |
| `storage` | string | yes | — | `local` or `remote` — where synced state lives. `remote` needs a `[remote]` in `config.toml`; checkpoint sync then pushes the tracker there. |
| `merge_with_source` | bool | no | `false` | meant to say whether a source merge carries this tracker's state with it. Accepted and stored, but nothing reads it yet: `newgit merge` merges every tracker the instance changed. |
| `paths` | array of strings | no | `[]` | workspace-relative paths the lane owns, normalized first (`./.env.local` is `.env.local`). Must not be absolute, contain `..`, or reach into `.git` or `.newgit` in any case (`.GIT` too), except that a code tracker may own other trackers' definitions, `.newgit/trackers/<name>.toml` (see `maintainers`); the rest of `.newgit` is `project`'s and `access`'s. `.` must be a tracker's only path. May be empty for a tracker that only receives resource deposits (`into_tracker`). |
| `secret` | bool | no | `true` | `false` makes a code tracker: committed with Git rather than captured. Written only when `false`. |
| `role` | string | no | — | the slot it fills: trackers sharing a role are alternatives (`dev-db`, `staging-db`) and may claim the same paths; an instance binds one of them. |
| `priority` | integer | no | `0` | which tracker of a role an instance binds by default (the highest), unless your own default says otherwise (`[projects.<project>.roles]` in `~/.newgit/config.toml`). Negative means only when asked for (`--use`), for a role-less tracker too. |
| `requires` | array of strings | no | `[]` | trackers or roles it cannot do without: binding it binds them, and they cannot be dropped while it is bound. |
| `resources` | array of strings | no | `[]` | resources it brings: they bind only while it is bound. `newgit tracker use` binds and prepares them and starts their `autostart` action; `drop` stops them and releases their binding. |
| `maintainers` | string | no | its `audience` | who may merge changes to it on a remote: `owner` (the project's owner), `project-devs`, or `user:<login>`. Changing a tracker's definition is changing the tracker that owns its file in `.newgit/trackers/` — `access` unless a more specific code tracker claims it — so governance layers: an `admin` tracker claiming `.newgit/trackers/web.toml` decides who changes `web`'s paths, audience, and maintainers. |

A secret tracker's paths stay out of Git: `newgit tracker track` adds them to
the store's `.gitignore`, and each workspace's Git is told to ignore them too,
so their content never lands in source history.

`newgit tracker remove <name>` deletes the definition and reverses both of
those: the `.gitignore` block and each live workspace's ignore entries. It
refuses while any live instance still has the tracker bound (`newgit remove
<instance>` first) and refuses outright for `root` and the code trackers, whose
histories are Git's. Captured history in `.newgit/local/trackers.git` is left
in place — it becomes unreferenced, and `newgit cleanup` reclaims it once
nothing else (a checkpoint that captured it) still pins a rev.

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
| `reads` | array of strings | no | unset | files and directories this resource's running process reads, workspace-relative (a directory covers everything in it). When `tracker use` or `drop` changes the bytes of a rendered file it covers, the process is restarted. See *Rendered files under a running process*. |

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
`workdir`. "Not an action" is a statement about `workdir` and about being
invocable by name — it is not a statement about the environment. They receive
the full command environment, same as an action; see *Command environment*.

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
| `produces` | array of strings | no | `[]` | the tree those inputs build — `node_modules` for an install. Same path rules as `paths`. |
| `key_command` | string | no | — | a command whose stdout is mixed into the identity key, for what `paths` cannot see. Rendered like an action `command` (see *Template variables*). |

Paths in both lists are workspace-relative with **no `.` or `..` components**
— one spelling per path, because every check here reads the first one.
`./.git` would otherwise lead with `.` and slip past the `.git` guard.

This is the single declaration of what the resource is derived from, and the
other two keys read it rather than repeating it:

- `[checkpoint] mode = "hash"` records the content hash of these paths. It has
  no `paths` of its own — two places stating the same fact is how they come to
  disagree.
- `[restore] mode = "recompute"` compares that hash against the workspace as it
  was before the undo, and **skips the rebuild when they match**. Same inputs,
  same tree; re-running would be an expensive no-op.

`paths` says what the resource is derived *from*. `produces` says what that
derivation yields, which is what lets the same tree be filled from a store
keyed on the inputs instead of rebuilt once per instance:

```toml
[identity]
paths       = ["package-lock.json", "package.json"]
produces    = ["node_modules"]
key_command = "node -v && uname -sm"

[actions.prepare]
command = "npm ci"
```

Paths are literal, here as everywhere in a definition — there is no globbing,
and a directory means everything under it. A monorepo that installs into
several places names each one.

Declaring `produces` is what puts a resource in the **install store**: at
spawn, an instance whose key already has a tree gets a copy-on-write clone of
it and the producing action does not run at all. See *Installs: use a
content-addressed store*.

`key_command` exists because the content hash of a lockfile describes what was
*asked for*, not what gets *built*. Install scripts bake in the platform and
the toolchain version, so two trees from one lockfile are not interchangeable
across an architecture or a Node bump. newgit does not guess at that list:
what a tree depends on beyond its lockfile is ecosystem knowledge the
definition has and newgit does not.

These are refused when you declare `produces`, all the same shape — a
declaration that could never key or fill an entry:

- **`produces` or `key_command` without `paths`.** A key over no content is
  the same key everywhere, so every instance would be handed an unrelated tree.
- **A `produces` that contains one of the `paths`**, or one that contains
  another. The first makes the key a function of the tree it keys; the second
  stores the same bytes twice.
- **Anything but a runnable `prepare` building it.** A spawn runs `prepare`
  and nothing else, so `prepare` is the only command an entry can stand in
  for: a `[restore] action` naming something else, a missing or command-less
  `prepare`, or a `long_running` one are all refused rather than left to
  silently never fill.
- **A `prepare` that declares `captures`.** Filling means the command does not
  run, so anything else it publishes would go missing with no warning
  anywhere. The tree has to be its whole output.

Installs are the usual reason for this section — see *Installs: use a
content-addressed store*.

### `[ports.<name>]`

One entry per port the resource needs. `<name>` is yours (`app`, `db`).

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `start` | integer | yes | — | where to start scanning. The allocated port is the first one from `start` upward that is neither promised to another instance nor claimed on 127.0.0.1, 0.0.0.0, ::1, or :: right now (checked with `SO_REUSEADDR` off, so a wildcard bind — e.g. a Docker-published container port — can't hide behind a loopback-only check; see #93). A family the OS doesn't support at all is skipped, not counted as free. This does not see a UDP-only listener on the same port number, and is a point-in-time check like any bind probe: nothing stops another process from claiming the port in the gap before it's actually used. |
| `env` | string | no | none | environment variable the port is published as to every command newgit runs — `newgit run`, actions, and the `[checkpoint]`, `[restore]` and `[cleanup]` hooks (e.g. `PORT`). |

A port is allocated once, at `spawn`, and recorded in the binding record. It
never changes for the life of the instance; removing the instance frees it.
Use it in templates as `{{ports.<name>}}`.

Unlike `[exports]`, order here does matter: `[ports]` is also a map, not a
sequence, but two ports whose ranges can reach each other (the normal case
for a tool with consecutive defaults) get different assignments depending on
which is allocated first. Ports within one resource are allocated in name
order, not declaration order — the same order `newgit resource list` and
spawn output already display them in, so what you see is what was used to
allocate. Do not rely on declaration order in the file; it is not read.

#### `newgit ports` / `newgit ports --check`

Bare `newgit ports` lists every instance's claimed ports straight off its
binding record — no probing, nothing to go stale, just the ledger newgit
already keeps.

`newgit ports --check` is the inverse of `render --check`: instead of asking
whether committed content matches a definition, it asks whether every
listening host port is claimed. It runs `lsof -nP -iTCP -sTCP:LISTEN`,
keeps only the ports inside some resource's allocatable range (`start` up to
the same 1000-port window `allocate` itself scans), and reports any of those
that no binding record claims:

```
$ newgit ports --check
ok    54321 — claimed by `newgit-trial`
FAIL  `newgit-trial` publishes 54324, 54327 — claimed by no binding record
      undeclared ports are not allocated, so another instance may be handed them
```

This is a cheaper mitigation than fixing the bind probe: `bindable` (the
check `allocate` uses to decide whether a port is free) only tries binding
`127.0.0.1`, so a container Docker Desktop has published on `0.0.0.0` is
invisible to it — that is a separate bug in the allocator itself. `lsof`
sees the real socket table regardless of which address a listener chose, so
`--check` catches the drift after the fact even when `spawn` could not have
caught it in the moment.

Naming *which* instance publishes an unclaimed port is done honestly, not
confidently: an instance is named only when its name or workspace path
genuinely appears in the listening process's command line. On macOS, a
Docker Desktop container's published port is fronted by Docker's own VM
proxy, whose command line says nothing about the instance that owns the
container — most Docker-caused conflicts therefore come back unattributed
(`FAIL 54330 — claimed by no binding record; could not attribute to an
instance`) rather than naming a guess. An unattributed conflict is still the
useful finding; a confidently wrong instance name would not be.

If `lsof` is missing, or exits with anything other than its own "nothing
found" code, `--check` says it could not check rather than reporting a
clean pass.

### `[actions.<name>]`

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `command` | string | yes, unless `signal` is set | — | shell command, run in the workspace (or `workdir`, below). |
| `workdir` | string | no | the resource's `workdir` | replaces — does not nest under — the resource-level `workdir` for this action only. |
| `long_running` | bool | no | `false` | supervise it: `newgit action` returns once started, output goes to a log, and the process group is tracked. Requires `command`. |
| `autostart` | bool | no | `false` | start it at `newgit spawn`, once every `prepare` has run. Requires `long_running`; at most one action per resource. See below. |
| `signal` | string | no | `term` for a `stop` action | makes an action signal-only. An action with `signal` and no `command` stops this resource's supervised process. The default applies to the literal name `stop` — see below, where that name is the one thing newgit reads. |
| `captures` | array of strings | no | `[]` | names to read out of the command's stdout and publish as this resource's exports. See *Captures*. |

**Actions are not lifecycle hooks.** Only `prepare` runs on its own — at
`spawn`, and again for a `recompute` restore — plus the one action a
resource marks `autostart`. Every other action is something you invoke:
`newgit action <resource>.<action> [instance]`. Add `--dry-run` to print the
rendered command and its directory without running it.

**`autostart` starts a process at spawn.** Once every resource is bound and
prepared, `newgit spawn` starts each `autostart` action, dependencies
first, exactly as `newgit action` would. One that is not ready, is blocked,
or depends on a resource whose start failed is not started, and the spawn
says so and exits 1, as it does for a failed `prepare`; `spawn --json`
lists each attempt under `started`. A dependent starts once its
dependency's process exists, not once it answers (#54).
After starting them, `spawn` waits up to five seconds for the ports of what
is running to answer. A server still compiling after that is `starting`, not
a failure: `spawn` exits 0, says so, and `spawn --json` lists it under
`starting`; `newgit status` shows `starting` until the port answers.
`spawn --from-checkpoint` starts them after its restore, and `newgit
tracker use` starts those of the resources the tracker brings, which `drop`
stopped. Nothing else autostarts: `newgit start` brings back exactly what
`newgit stop` stopped.

**`stop` is the one name newgit reads for signals.** Names are otherwise
inert — `start` is pure convention, made real by `long_running` and not by
the name, and `suspend` / `resume` are covered below — but when newgit needs
to stop this resource's supervised process on its own (during `remove`,
`undo`, `newgit stop`, and `resource remove --force`) it signals, and it
looks up `[actions.stop] signal` to decide which signal to send, defaulting
to `term`. Precisely:

- An action named `stop` **with no `command`** is the signal-only case the
  `signal` default is written for. Its `signal` is what newgit sends, both
  when you invoke it and when newgit stops the resource itself.
- An action named `stop` **with a `command`** is an ordinary action. It is
  accepted, and invoking it runs the command like any other. But newgit's
  own stops always signal — they never run an action's command — so such an
  action is never reached that way, and the resource gets a bare `term`
  because no `signal` was set. If your teardown is a command rather than a
  signal (`docker compose stop`), it belongs in `[cleanup]`, which newgit
  does run, or under any other name you invoke yourself.
- Any other name is inert unless you invoke it. An action named `halt` with
  `signal = "term"` is a perfectly good signal-only action; it just is not
  what newgit looks up when it stops things by itself.

**`suspend` and `resume` are read by `newgit stop` and `newgit start`.**
`newgit stop [instance]` takes an instance's resources down without removing
anything, dependents first: it signals each supervised process (as above),
then runs `suspend`. `newgit start [instance]` brings back exactly what the
last stop took down, dependencies first: it runs `resume`, then starts the
long-running action again. A resource that was not running at the stop stays
down, and one whose dependency did not come back is not started against it.
The workspace, trackers, port claims and installs are left alone, so the
instance comes back on the same ports with its rendered config still valid.

`suspend` / `resume` are for a resource that keeps running without a
process newgit supervises: a `prepare` that starts a Supabase stack, a
Compose project, a container, or a cloud machine that bills while it idles.
Each must have a `command`, cannot be `long_running`, and neither is
accepted without the other. `suspend` runs only for a resource that is
`ready` and whose ownership lets per-instance teardown touch it (see
*Ownership*), so stopping one instance never suspends something other
instances share. A failed `suspend` is reported and not recorded, and the
next `stop` tries again; a failed `resume` stays recorded, and the next
`start` tries again. Unlike `[cleanup]`, `suspend` must not destroy
anything: `resume` has to find the resource where `suspend` left it. Like
any action, either may declare `captures`, and a `resume` that hands out a
new URL publishes it that way.

`newgit tracker drop` (and a `tracker use` that switches a role) runs the
same pair for the resources the dropped tracker brought: it *parks* each,
stopping its process and running `suspend`, keeps its ports claimed, and
the next `use` hands them back and runs `resume`. A resource with no
`suspend` is parked by its `[cleanup]` instead, and `use` runs `prepare`
on the same ports. Either way the output names the hook that ran, and
`newgit remove` runs `[cleanup]` for a parked binding its drop did not
already clean up.

```toml
[actions.suspend]
command = "docker compose -p {{branch.slug}} stop"   # not `down`: that is cleanup

[actions.resume]
command = "docker compose -p {{branch.slug}} start"
```

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

**A `command` checkpoint should produce the same bytes for the same state.**
The state ref is how `newgit checkpoint --verify` tells whether a restore
landed where it started, so a command that stamps each run differently makes
every verify fail. Common dump tools do this by default: `pg_dump` since
17.6 (and the matching back-branch releases) opens and closes every dump with
`\restrict <token>` / `\unrestrict <token>`, a fresh random token per run.
Pass `--restrict-key=<fixed>` to make it stable, as the bundled Postgres
templates do (their comments say what a fixed key gives up). Anything else
that stamps a run does the same: a timestamp in the output, or `pg_dump
--verbose`'s `-- Started on` / `-- Completed on` comments. `--verify` names
this case when it happens rather than blaming `[restore]`; it does not paper
over it.

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

A `[restore]` command is also refused *at undo* if its `{{state_ref}}` has no
value in the checkpoint being restored, and the undo counts that resource as
not restored; a `[cleanup]` hook is refused *at teardown* the same way — see
`[cleanup]`. The load-time rules above cannot catch everything the runtime ones
do: a checkpoint record written under an older definition, or by a `command`
checkpoint that printed nothing, is not something reading the current file can
predict.

A checkpoint goes ahead without a resource that is not up and whose capture
fails, and names it on a `degraded:` line. An undo to it does not run that
resource's restore when it is a `command` reading what a `command` or
`external` checkpoint records, since there is no dump or handle to read: the
resource keeps its state and status. A process it was running is still
stopped with the others while content changes, and started again after its
dependencies; the undo then reports it as `not restored + restarted`. Any
other restore runs as usual and misses nothing it needs: a `recompute`
rebuilds from the restored source and ignores the ref.

A `recompute` restore skips when this resource's `[identity]` hash is the same
now as at the checkpoint and, for a resource that declares `[identity]
produces`, the undo did not move the part of the binding its store key holds:
the tree was already built from those inputs, so the rebuild would change
nothing. That part counts because the install store keys a tree on it (see
*Installs: use a content-addressed store*); an undo that drops or brings back
a code tracker rebuilds such a tree, lockfile or no lockfile, while one that
only rebinds a secret tracker does not. It says so rather than passing
silently:

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

A restore command runs against whatever state its own reset left behind,
including rows a migration step already inserted — it is not a fresh
database unless the command made one.

**Restore is unproven until it has actually run.** `[checkpoint]` runs the
day it is written; `[restore]` runs the day it is needed. A `command` or
`recompute` restore that has never completed successfully on this branch
instance gets `— restore never exercised` on its `newgit checkpoint` line;
`none` and `external` run nothing, so there is nothing to prove. A real
`undo` — not a `recompute` skip, which never touches the command — is what
clears it, and it stays cleared even if a later restore fails: the claim is
"has this ever completed", not "would it complete right now".

`newgit checkpoint --verify <instance>` proves it without waiting for a
real rollback: checkpoint, `undo` back to that checkpoint, checkpoint
again, and diff the two runs' resource state refs. Agreement is a stronger
claim than "the command exited 0" — a restore can exit clean and still
land on the wrong state, which `--verify` is what catches.

A mismatch has two opposite fixes, and `--verify` says which one it saw.
It checkpoints the mismatched resource once more, straight after the
second checkpoint with nothing restored in between: if that reproduces the
second ref, the checkpoint reproduces itself back to back, so `[restore]`
most likely landed on a different state; if it does not, `[checkpoint]` is
not deterministic and this run could not judge the restore at all. A restore that failed outright is named as such
without the extra run. Either way the verify still fails — this is evidence
for whoever fixes it, never a reason to call a mismatch verified. The extra
checkpoint is paid only on a mismatch, and only for that resource.

When the refs are `into_tracker` deposits, it also shows what differs —
each file, its changed line counts, the first few changed lines (control
characters escaped) — and the `git diff --no-index` command over both
snapshot directories for the rest. Which two it compares depends on the
cause: `before` → `after` (what the restore changed) normally, but `after` →
`control` for an unstable checkpoint, since nothing was restored between
those two and every line that differs is the checkpoint's own noise:

```
resource: db MISMATCH  before: tracker:db-snapshots@a71acd9dcafa  after: tracker:db-snapshots@f5b2685a1347
          cause: [checkpoint] is not deterministic — checkpointed again with nothing restored in between, it gave tracker:db-snapshots@00f1294c80fb, not `after`. ...
          after → control (nothing was restored between these two):
          db.sql: -1 +1 lines (of 3300)
            -\restrict 4fQx9ZkTmB2vLr7HcWn0aYdE8sJpU3gNi6KoVbXt1RzA5yCeqM
            +\restrict Ty7Nw2cLpQ8vZk0RbJmX4hSaFe6UdG1oYi9KtrWnE3sDx5HjBu
          full diff: git diff --no-index .newgit/snapshots/db-snapshots/f5b2685a1347 .newgit/snapshots/db-snapshots/00f1294c80fb
```

The comparison stays bounded however large the dump: one `git diff
--numstat` for the counts, and one patch streamed only until the excerpt is
full. The control's own log is `<resource>.checkpoint-control-*.log`.

It is destructive (a real `undo`, stopping and restarting whatever the instance's
resources run) and prints what it is about to do before doing it; run it
against an instance you can afford to spend, not one you are mid-task on.

### `[cleanup]`

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `command` | string | no | none | tears the concrete resource down. Runs at `newgit remove`, and at `newgit cleanup` for an instance whose workspace is gone, dependents before dependencies — for resources a tracker drop parked too. A `tracker drop` runs it for a resource it parks that has no `suspend`. A `[cleanup]` section without one means there is nothing to do, and teardown says so. |

Three rules constrain it, all deliberately:

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
- **A name with two values refuses.** Teardown is the way out of a resource
  graph that does not resolve, so it runs on one, and its hooks get the
  environment layered in whatever order the graph has. The exception is a
  name two declarations claim, when both reach the instance with different
  values, as when a port's `env` is renamed onto a name another of its
  resources sets: the hook would get whichever is layered last, so
  `newgit remove` and `newgit cleanup` refuse to tear the instance down,
  naming the collision. A collision that does not reach the instance, such
  as one with a resource added after the spawn, does not block it.

### `[exports]`

`NAME = "value"` pairs, rendered once at `spawn` and stored in the binding
record. They become environment variables for every command newgit runs in
this instance — `newgit run`, every resource's actions, and the
`[checkpoint]`, `[restore]` and `[cleanup]` hooks, which are not actions but
are commands — and are readable in later hooks as `{{exports.NAME}}`. The
environment is per instance, not per resource: one name has one owner (see
*Command environment*), so there is nothing to scope.

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

**Exports are bound before this resource's own `prepare` runs.** `spawn`
binds one resource at a time in dependency order, and within one resource the
order is: allocate `[ports]`, render `[exports]`, apply `[[render]]`, then
run `prepare`. So a `prepare` may consume its own resource's exports as
environment variables, and a dependency's exports were bound earlier still,
in the previous iteration. Both "at `spawn`", with no order stated between
them, is not enough to write a `prepare` against: a `prepare` that brings up a
Compose stack named by its own `{{branch.slug}}` export would otherwise have
to guess, and guessing wrong brings up the *shared* stack under the committed
default name — no error, correct-looking output, wrong cluster. Restating the
value inline in every hook to avoid the question is the duplication this
reference warns against elsewhere; you do not have to.

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

**A data edge does not block.** If the resource owning an export fails to
`prepare`, its readers are not held back the way a `depends_on` dependent is
— and they do not need to be, because of what a static `[exports]` value can
be made of: ports, branch vars, `{{workspace}}`, `{{scripts}}`, and other
exports. None of those depend on `prepare` succeeding. A port is allocated to
the instance whether or not the process ever came up, so the value is
correct, not stale.

Anything that genuinely depends on `prepare` has to arrive through
`captures`, and a capture that never arrived leaves `{{exports.<name>}}`
unresolved — which `[exports]` and `[[render]]` already refuse, stopping the
reader without any blocking rule. Blocking on a data edge would instead
withhold a correct render because an unrelated process failed to start, which
is the over-claiming this section exists to remove.

Resources export runtime values — ports, URLs, handles. Trackers do not
export anything; they own file content.

### `[[render]]`

Substitutes this instance's values into a file the project commits — for the
common case where a tool reads its port from a config file rather than argv.
Repeatable: one `[[render]]` per file.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `path` | string | yes | — | workspace-relative path to the file. Must be tracked by Git or owned by a tracker: a render substitutes into committed content, so there has to be some. The exception is a file inside this resource's own `[identity] produces` — see *Rendering into a built tree*. Two resources rendering one path is refused at load. |
| `replace` | array of tables | yes | — | the substitutions, below. |

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `find` | string | yes | — | **a literal string, never a regex.** May span lines. |
| `with` | string | yes | — | what to put there; rendered with the variables below. |
| `count` | integer | no | `1` | how many times `find` is expected to occur. Refused on a render into a built tree. |

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

  A `find` that occurs 0 times often means the file has no line for that
  setting at all, such as a `supabase/config.toml` without `[inbucket]`. The
  tool then uses its own built-in default, and every instance shares it. The
  error says so. Add the line to the committed file so there is one to
  rewrite; deleting the rule makes the bind pass and leaves the collision.
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

#### Rendered files under a running process

A process reads its config when it starts. When `newgit tracker use` or `drop`
changes the bytes of a rendered file — binding a tracker's `db` writes its port
into `apps/admin/.env.local` — every process that was already running is
checked against the resource's `reads`:

| `reads` | the process |
| --- | --- |
| covers a changed file | is restarted (stopped with its `stop` signal, started again with its `autostart` action, else its first long-running one), and listed under `restarted:` |
| set, but covers none | is left alone, and not mentioned |
| unset | is left running, with a warning naming it and the command that restarts it |

```toml
reads = ["apps/admin"]   # next dev, run from apps/admin, reads apps/admin/.env.local
```

`reads = []` says the process reads no rendered file. A change that leaves
every rendered file's bytes as they were says nothing. `undo` stops every
process before it moves content and restarts what was running at checkpoint
time, so it does not need `reads`.

#### Rendering into a built tree

Some installs write the workspace's own absolute path into their output —
pnpm records every workspace project's path in
`node_modules/.pnpm-workspace-state-v1.json`. A tree like that is never shared
by the install store (see *What it costs, and what it cannot do*), unless a
render says which file names the workspace:

```toml
[[render]]
path = "node_modules/.pnpm-workspace-state-v1.json"
replace = [{ find = "/__newgit_workspace__", with = "{{workspace}}" }]
```

A render whose `path` is inside the same resource's `[identity] produces`
treats the file *as the tool wrote it* as already rendered, for the instance
that built it. There is no committed content to read, so the stored tree takes
its place:

- **Storing reverses it.** Every occurrence of the building instance's
  `{{workspace}}` becomes `find` in the stored copy, and that copy is then
  scanned like the rest of the tree: a spelling of the path the render does
  not cover still keeps the tree out of the store. `find` is any string that
  does not otherwise occur in the file.
- **Filling renders it forwards.** Every `find` becomes this instance's
  value. After a real install there is nothing to do — the tool already
  wrote the right path.
- **`count` is refused.** The occurrences are the ones the build wrote, one
  per workspace project here, and the number moves whenever a project is
  added; a declared count would only break the fill. Drift is the scan's
  job: a build that starts writing the path somewhere else is declined, and
  one that stops writing it is reported on the store line.
- **`render --check` skips it**: the file is not in the working tree until
  an install builds it.

`{{workspace}}` is the path as newgit spells it. A tool that records the
*canonical* path instead (`/private/var/...` where the workspace is
`/var/...`) is not matched, and the scan declines that tree.

**Adopting a `[[render]]` means committing the default it substitutes into
before anything can test it.** The rule above — committed content is the
input — means a `find` you just typed does not exist in the content newgit
would render until you commit it. `newgit render --check` is the fix: it
resolves every `[[render]]` against the working tree instead, with no
instance, no spawn, and no commit, and reports per file which `find` matched
and which did not:

```
$ newgit render --check
ok    supabase: `packages/db/supabase/config.toml` — `port = 54321` (1)
FAIL  supabase: `packages/db/supabase/config.toml` — `port = 54324` expected 1, found 0
```

A failing check exits non-zero, so it also runs as a drift detector in CI: a
default the upstream tool changed fails the build there instead of the next
`spawn`. It never writes or substitutes anything — `with` is not even
resolved, since a `find` that fails to match fails whether or not there is a
port to put in its place yet.

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

Every branch instance gets its own dependency tree. That is not newgit being
wasteful: two branches with different lockfiles must not share one, or one
branch's install silently rewrites the other's. It is why installs are
resources with `[identity]` rather than trackers — the lockfile is the truth,
and the tree is derived from it.

But two instances at the **same** lockfile are not two derivations. They are
one tree, built twice. Declare what your install produces and newgit builds it
once:

```toml
[identity]
paths       = ["package-lock.json", "package.json"]
produces    = ["node_modules"]
key_command = "node -v && uname -sm"

[actions.prepare]
command = "npm ci"
```

The first instance of a given key installs normally and its tree is published
to `.newgit/installs/`. Every later instance of that key is filled from it and
**does not run the install at all**:

```
  resource:  `deps` prepare: ok
             install store: stored as 1264badb0621 (copy-on-write)

  resource:  `deps` prepare: not needed
             install store: filled from 1264badb0621 (copy-on-write)
```

Nothing about the hard rule changes: a different lockfile is a different key
is a different tree. The store only ever hands an instance a tree built from
inputs that hash the same as its own.

### What it costs, and what it cannot do

**Copies are copy-on-write, not hardlinks.** This matters more than it looks.
A `node_modules` is not read-only after install — native builds write into it
(`.cxx` caches, compiled artifacts), and under hardlinks one instance's
Android build would rewrite every other instance's tree. Under copy-on-write
that write forks and nobody notices. Where the filesystem cannot clone (APFS
and btrfs/XFS can; many cannot) newgit takes a **full copy** and says so on
the line. Same behaviour everywhere, worse performance — and on such a
filesystem an entry costs a whole tree on disk rather than almost nothing.

**A tree that names its own workspace is never shared.** Some postinstall
scripts bake their absolute location into a file. Copy-on-write cannot fix
that — the wrong path is already in the content before anyone writes to it —
so newgit scans a tree before publishing it and declines the ones that are not
relocatable, naming the file:

```
  install store: not stored — the built tree names its own workspace in
  `.../node_modules/sharp/config.json`, so it cannot be shared (key 3ec46aa385bb)
```

That resource just keeps installing per instance, exactly as it did before.
Nothing fails. When the file only records the path as data — pnpm's
workspace state, for one — a `[[render]]` on it lets the store rewrite it per
instance instead; see *Rendering into a built tree*. The `pnpm` template
declares that one.

**`key_command` is not optional in practice.** A lockfile hash describes what
was asked for, not what gets built: install scripts compile against the local
platform and toolchain. Point it at whatever actually varies — `node -v &&
uname -sm` is the usual answer for JS.

**The definition file is part of the key.** The command that builds a tree is
as much an input as the lockfile it reads, so editing `npm ci` to `npm ci
--omit=dev` moves the key and the next instance rebuilds. The cost is that
*any* edit to the file moves it — a comment, an unrelated port — orphaning the
entries built before it. That is the right way to be wrong: a stale entry
costs one reinstall and `newgit cleanup` reclaims it.

**So is what of the binding can reach the install.** A shared lockfile lists
every member, bound or not, and its install builds the ones it can see: found
on disk, where a dropped code tracker's files are not checked out (an
unfiltered `npm ci` links a workspace member only when it is there), or named
by `{{bound.paths}}`. So two things are in the key:

- **Which paths are checked out**, in every resource's key: the paths the
  instance's dropped code trackers leave out. Nothing is left out when no
  code tracker is dropped.
- **What `{{bound.trackers}}` and `{{bound.paths}}` render to**, only when
  the install can be told: its own commands, `key_command`, exports or
  `[[render]]`s name one, or any resource's export or `[[render]]` does
  (every command's environment carries every bound resource's exports, and a
  render writes into a file the install can read).

The store never fills an instance that drops a member with the tree built for
one that binds it. And the rest of the binding stays out: binding a secret
tracker, or one that owns no paths (a branch database, say), or creating or
renaming a tracker the instance binds, leaves an install that is not told the
names at the key it had, so instances that differ only in that share one
tree. A secret tracker's files are projected into the workspace but are not
in the key; an install that reads one (an `.npmrc` it owns, say) names it in
`[identity] paths`, whose content is hashed like the lockfile's.

A live instance whose key moves needs a new tree: `newgit tracker use` and
`drop` rebuild every bound install that holds a tree and whose key the change
moved, filled from the store when that binding was built before, else by
running its `prepare` again, and say so. An install the change cannot reach
is left alone and not listed:

```
Bound `b` in `x`
  deps for the new binding: rebuilt, install store: filled from 3ec46aa385bb (copy-on-write)
```

Running processes are left as they are, as under `newgit action deps.prepare`. The binding has changed by
the time an install rebuilds, so a rebuild that fails does not fail the
command. It leaves the resource `failed`, and what depends on it is not
rebuilt but left `pending` behind it, each with its retry:

```
Bound `b` in `x`
  deps for the new binding: rebuild failed; see .newgit/logs/x/deps.prepare-….log, then `newgit action deps.prepare`
  dev for the new binding: not rebuilt, waiting on deps; then `newgit action dev.prepare`
```

When newgit cannot run the action at all (its `workdir` left the working tree
with the tracker, say), the line gives that error in place of the log.

**An undo is filled from it too.** The rebuild a `recompute` restore triggers
is the expensive one — a full reinstall in the middle of an operation someone
is waiting on. Undo restores source first, so by the time the resource is
restored the workspace holds the checkpoint's lockfile, and anything the store
has under that key *is* the tree the rebuild would produce:

```
resource: deps recompute(prepare) filled from install store 3ec46aa385bb (copy-on-write) ok
```

The existing tree is moved aside rather than deleted, and discarded only once
the clone has landed: a copy that fails partway through leaves the instance
with the stale tree it started with, never with none.

`newgit undo --force-recompute` bypasses all of it. That flag means "do not
trust that the identity describes the tree" — usually because the tree is
damaged in a way the lockfile cannot show — and a cache keyed on that identity
is under exactly the same suspicion, so the flag **drops the entry** and the
rebuild republishes it. That is the way out if an entry is ever wrong.

**The store is a cache, and `newgit cleanup` prunes it.** An entry survives as
long as some live instance's identity still keys to it; the trees of lockfiles
everyone has moved past are dropped, along with anything a killed `newgit
spawn` left half-copied. Nothing in the store is data — dropping an entry costs
a reinstall.

Pruning is per resource. An instance whose key cannot be computed withholds
that resource's entries and says so, rather than disabling the sweep for the
whole project. `newgit cleanup --dry-run` will not run a `key_command` at all
— a dry run observes, and spawning a user-supplied shell is acting — so
resources that need one are reported as unevaluated instead of guessed at.

**Produced trees are kept out of source.** A workspace's Git is told to ignore
every declared `produces` path (`.git/info/exclude`, per clone), the same way
tracker-owned paths are. Without it, a project with no `node_modules` rule of
its own would have its whole install swept into source history by the `git add
-A` a checkpoint runs. The rules are written at spawn and re-synced at
checkpoint, so an instance that predates the declaration is covered too.

**It is not a security boundary and not a sandbox.** An install script that
writes outside its own tree is doing that on every instance anyway.

### Your package manager still matters

What a *miss* costs is set by your package manager, not by newgit:

| tool | per-instance cost | why |
| --- | --- | --- |
| pnpm | directory entries | hardlinks packages from one global store |
| Yarn PnP | ~nothing | no install tree at all |
| uv, bun | directory entries | hardlink from a shared cache |
| Cargo | a `target/` each | the registry is shared; build output is not |
| npm, Yarn classic | a **full copy** each | the cache holds tarballs, so `npm ci` re-expands every time |
| pip into a venv | a **full copy** each | same shape |

The install store flattens most of this — ten instances of one lockfile is
one tree under any of them — but it only helps on a hit. Ten instances of ten
*different* lockfiles still pays ten installs, and there the package manager
is the whole story. If you are adopting newgit and have the choice, it is
still worth making here.

Two more things matter when you wire an install up:

- Give the shared store its own resource with **`ownership = "user"`**, and
  have the install `depends_on` it. `user` is the one ownership newgit never
  deletes, at `remove` or at `cleanup` — correct, because that store is shared
  with every other project on the machine. The `pnpm` template ships this pair
  already.
- Point the install's **`[identity] paths`** at the lockfile *and* the
  manifest, so a `recompute` restore reinstalls exactly what the checkpoint
  described.

If you cannot switch package managers, nothing breaks — it costs disk on a
miss. Run fewer concurrent instances, and let `newgit cleanup` reclaim both
the trees of instances whose workspaces are gone and the store entries nothing
keys to any more.

`newgit resource templates --show pnpm` is the worked example above, wired
for pnpm specifically. For anything else — npm, uv, Cargo, or a package
manager not listed here — start from `newgit resource templates --show
install` instead: the same shape with no `depends_on` and no store, and its
lockfile and install command marked `EDIT ME` rather than guessed wrong.

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
| `{{branch.name}}` | the instance name as you typed it (at most 40 characters: the slug lands in container names, Supabase project ids, and hostnames) |
| `{{branch.slug}}` | its filesystem-safe form, and the workspace directory name |
| `{{workspace}}` | absolute path to the workspace |
| `{{scripts}}` | absolute path to `.newgit/scripts/` in the store — use `{{scripts}}/<name>` so a hook's script is read from the store, like the definition calling it |
| `{{ports.<name>}}` | an allocated port |
| `{{exports.<name>}}` | a rendered export from this resource's binding |
| `{{snapshot.path}}` | directory to write checkpoint output into |
| `{{state_ref}}` | the handle the last checkpoint recorded, or the path of the snapshot it deposited. A `hash` checkpoint records neither, so it has no value |
| `{{bound.trackers}}` | the trackers the instance binds, space-separated — every selectable tracker it has not dropped (`root`, `project`, and `access` are always bound and not listed) |
| `{{bound.paths}}` | the paths those trackers own, code trackers only, space-separated — what a shared install should build |

Scope — which of them have a value where:

| rendered in | branch/workspace/scripts | `bound.*` | `ports.*` | `exports.*` | `snapshot.path` | `state_ref` |
| --- | --- | --- | --- | --- | --- | --- |
| `[exports]` values | yes | yes | yes | yes | — | — |
| `[[render]]` `with` | yes | yes | yes | yes | — | — |
| action `command` | yes | yes | yes | — | — | — |
| `[identity] key_command` | yes | yes | yes | — | — | — |
| `[checkpoint] command` | yes | yes | yes | yes | yes | — |
| `[checkpoint] state_ref` | yes | yes | yes | yes | — | — |
| `[restore] command` | yes | yes | yes | yes | — | yes |
| `[cleanup] command` | yes | yes | yes | yes | — | yes |

A `key_command` is rendered exactly like an action `command`. Naming
`{{bound.*}}` in it puts what it renders to in the key (see *Installs: use a
content-addressed store*), as naming it in `prepare` does; mostly it lets the
command pass the binding to whatever it inspects, as in `git hash-object
{{bound.paths}}`. Keep per-instance values —
`{{workspace}}`, `{{branch.*}}`, `{{ports.*}}` — out of what it prints. Every
instance would key differently, and the shared store would never hit.

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

Every command newgit runs sees:

1. each resource's rendered `[exports]`, in dependency order;
2. `[ports.<name>] env` variables;
3. `NEWGIT_BRANCH` and `NEWGIT_WORKSPACE`.

"Every command" means every command, not just actions. `[checkpoint]`,
`[restore]`, and `[cleanup]` are not actions — they have no `workdir`
override of their own and you cannot invoke them by name — but they are
commands, and they get the same environment an action gets:

| rendered in | `[exports]` | port `env` | `NEWGIT_*` |
| --- | --- | --- | --- |
| `newgit run` | yes | yes | yes |
| action `command` | yes | yes | yes |
| `[checkpoint] command` | yes | yes | yes |
| `[restore] command` | yes | yes | yes |
| `[cleanup] command` | yes | yes | yes |

This is worth stating rather than leaving to inference, because the hook it
matters most for is the one you least want to guess at. A `[restore]` that
resets a database picks *which* database from the port newgit allocated; if
it did not receive that variable it would fall back to whatever its committed
default names — usually the developer's shared local database — and destroy
the wrong state, quietly and plausibly. Every variable a `[restore]` needs is
there.

`newgit env [instance]` prints it, one `export NAME=value` line per variable
with the declaration it came from, so a destructive hook's inputs can be read
instead of discovered by running it:

```
export DB_PORT=5434                            # db [ports.pg]
export DB_TOKEN=tok_123                        # db [actions.mint] captures
export DATABASE_URL=postgres://...:5434/feat-x # db [exports]
export NEWGIT_BRANCH=feature/x                 # newgit
export NEWGIT_WORKSPACE=/Users/.../feature-x   # newgit
```

It is the same assembly the hooks use, not a reconstruction of it, and it is
valid shell: `eval "$(newgit env)"` sets and exports every variable, so the
programs that shell starts see them as a hook would. A name a shell cannot
assign (`NEXT-PUBLIC-URL`) is listed commented out rather than dropped:
newgit sets it for the hook's `sh`, but whether the programs that `sh`
starts see it depends on the shell (bash passes it on, dash drops it), so
use names a shell can assign. One case it cannot show in advance: during an
`undo`, a `recompute` restore that captures a new value hands it to the
`[restore]` hooks after it in the same undo. Values are printed verbatim: a
credential a resource exports appears in plain text, exactly as a hook
receives it. What it leaves out is the environment of whoever runs newgit,
which every command inherits underneath these variables. The command itself
— the other half of "what is this about to do" — is `newgit action
<resource>.<action> --dry-run`.

Note that this is the opposite shape to `{{...}}` substitution, where scope
genuinely differs per site (see *Template variables*). The environment does
not vary: there is one environment, and every command gets all of it.

Trackers contribute no environment; they place files. Loading `.env`-style
files is command-run policy, not a tracker feature. A `[[render]]` gets no
environment either — it writes a file, and nothing hands a file an
environment; it reads the same values through `{{...}}` instead.

**A name has exactly one owner.** Three things declare an environment
variable — an `[exports]` key, an action's `captures` entry, and a port's
`env` — and the same name appearing in two of them is a graph problem,
reported by `newgit resource list` and `newgit render --check`, and refused by `spawn`, `run`, `action`,
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

`NEWGIT_BRANCH`, `NEWGIT_WORKSPACE`, and `NEWGIT_LOCKED` are reserved, so
a resource declaring one is reported too — a declaration that could never
reach the process is a mistake worth naming, not a silent no-op. newgit sets
the first two for every command it runs; `NEWGIT_LOCKED` it sets only for a
hook run under an instance's lock.

Every command that changes an instance (`spawn`, `action`, `checkpoint`,
`undo`, `merge`, `tracker` binding changes, `remove`, `cleanup`) holds that
instance's lock, `.newgit/local/locks/<slug>.lock`, from start to finish,
so two of them on one instance run one after the other rather than over
each other; the second says it is waiting. A hook it runs gets
`NEWGIT_LOCKED`, a comma-separated list of `<slug>:<token>`, one entry per
lock held by that command or by one it was itself started from. A `newgit`
started from such a hook that would need one of those locks — a
`[checkpoint] command` running `newgit checkpoint` on its own instance, or
on another instance whose hook comes back to this one — refuses at once and
says so, instead of waiting for its own ancestor forever. The token is
written into the lock file when the lock is taken and cleared when it is
let go, so a process that outlives the command which started it (a dev
server) carries a token that matches nothing, and waits like any other
command. Not caught: two independent commands whose hooks each run
`newgit` on the other's instance; both wait until one is interrupted.
`newgit run` holds no lock and does not set it.

A lock file is left in place when its lock is let go, and `cleanup` deletes
those of instances with no record. Records are written to a temporary file
beside them and renamed into place; a command killed between the two can
leave a `.<random>.tmp` file there, which nothing reads and which is safe
to delete.

This is why the starter templates do not ship conventional names like `PORT`
or `DATABASE_URL`. Two services in one project cannot both publish `PORT`,
so `newgit resource add web --template process` writes `WEB_PORT` and
`WEB_URL`, naming them after the resource. Rename them if you have one
service and your tool insists on `PORT` — the generated file says so.

---

## Export — `.newgit/exports/<name>.toml`

Somewhere `newgit export --remote <name>` pushes: an ordinary Git
repository, usually a GitHub mirror, that receives the trackers' `main`s
composed into one history. Real authors, dates, and messages, so the result
is an ordinary repository that `git clone`, `gh`, and GitHub's UI read like
any other. Write one by hand; one file per target, so a public mirror and a
private one can each show a different view of the same project. A target
may not be named `origin`, since its name is also a remote's in each
workspace.

```toml
# .newgit/exports/github.toml
url = "acme/widget"
trackers = ["root"]
```

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `url` | string | yes | — | where to push. `owner/repo` means `https://github.com/owner/repo.git`; anything else (an `https://` or `git@` URL, a path) is used as written. Pushed with plain `git push`, so your own Git credentials apply (`gh auth setup-git` sets them up for GitHub). |
| `branch` | string | no | `main` | the branch the export is pushed to. |
| `trackers` | array of strings | no | `[]` | code trackers to ship besides the `public` ones. A secret tracker here is an error: secrets never leave through Git. |
| `branches` | bool | no | `false` | also export each instance's branch, every time its workspace pushes, so a pull request can be opened against the target (see below). |

What ships fails closed. Only code trackers whose audience is `public` ship
unless `trackers` names them, and `root`, `project`, and `access` are
`project-devs`. So `trackers = ["root"]` mirrors the project's code without
`.newgit/`, and adding `"project", "access"` mirrors that too. A path owned
by a tracker that does not ship stays out of every exported commit. A
history that ever adds a path a secret tracker owns, such as a `.env`
committed before it was tracked, refuses the whole export before anything
is pushed.

The export is composed from each tracker's `main`: on a newgit remote, the
remote's; otherwise this project's. Work committed in the project root
reaches `main` with `newgit merge main`, and the export says when it is
leaving such work behind. Composing is deterministic. Exporting again
fast-forwards the target, and while every tracker that ever touched a commit
ships, its id is the original's. Dropping a tracker from the set (or a
target that already has history of its own) rebuilds commits under new ids,
and the push is refused until `--force` replaces the branch. `--dry-run`
composes and checks but pushes nothing.

A pull request merged on the target, or anything else pushed straight to
its branch, comes back before the push. When the target's branch is ahead
of the export, the project root's branch fast-forwards to it and merges into
`main`, as `git pull` then `newgit merge main` there would, and the export
pushes what results. That works while the target's commits are the
project's: the project root has no commits of its own, and the target ships
every tracker those commits touch. Otherwise the export refuses and says how
to bring them in by hand. `--dry-run` says how many commits would come in;
`--force` replaces the branch instead.

### Pull requests from a workspace: `branches = true`

On a newgit remote a workspace's `origin` is the newgit endpoint, which `gh`
cannot map to a GitHub repository. A target with `branches = true` fills
that gap:

- **Each push from a workspace exports the instance's branch** to the
  target as it sees it (the same trackers as its `main`), after the push
  itself has succeeded. A failure there is a warning in the push's output,
  not a failed push. A force-push follows a force-push.
- **On a newgit remote, the target is a remote in every workspace**, named
  after it, and the first such target is the one `gh` resolves
  (`gh-resolved = base`). `refs/remotes/<target>/<branch>` is set to the
  commit the target really holds. A non-interactive `gh pr create` finds the
  branch only when that ref is `HEAD`. That is true when the target ships
  every tracker the branch's history touches, and then the push says
  `gh pr create` works as is. Otherwise the commits there are rebuilt
  without the withheld trackers, and the push says to run
  `gh pr create --head <branch>`.
- **Pushing to the target's remote directly is refused**, naming `git push
  origin`: it would skip the checkpoint and could carry trackers the target
  withholds.

Without a newgit remote, a workspace's `origin` is already the real
repository; `branches = true` still exports each pushed branch, but adds no
remote.

---

## Project — `.newgit/config.toml`

Written by `newgit init`.

| key | type | required | default | meaning |
| --- | --- | --- | --- | --- |
| `project.name` | string | yes | the repository directory name | used in the default workspace root. |
| `project.source` | string | yes | detected | `git`, or `jj` for a jj repository colocated with Git. newgit drives source through Git either way, and refuses a jj repository with no `.git`. |
| `workspace.root` | string | no | `~/.newgit/workspaces/<name>-<hash>/` | where workspaces are materialized. `~` expands. Omitted from the generated file so a committed config never bakes in one user's absolute paths. |
| `workspace.materializer` | string | no | real directories | reserved; every workspace is a full clone. |
| `remote.url` | string | yes, in `[remote]` | — | the newgit remote, e.g. `https://newgit.dev`. Must be `https://`, except `http://localhost` and `http://127.0.0.1` for a local `wrangler dev`. Written by `newgit remote add`. |
| `remote.project` | string | yes, to link | — | the project on the remote, `<owner>/<name>`. Without it the project is not linked: newgit acts as if there were no `[remote]`. Written by `newgit remote add`. |

Your own settings live in `~/.newgit/config.toml`, which newgit never
writes. The one it reads is your default tracker per role, which beats the
role's `priority` when you spawn. `<project>` is the `[project] name`:

```toml
[projects.<project>.roles]
db = "staging-db"
```
