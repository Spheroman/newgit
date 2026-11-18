# newgit design

*Why newgit is shaped the way it is: the model, the decisions behind it, and
what they cost.*

This document describes newgit as it exists on `main`. It explains the
design, not the commands. For those, and for the rest of the documentation:

- [The wiki](../wiki/README.md) covers how to use newgit, command by command.
- [`definitions.md`](../../crates/newgit/reference/definitions.md), which
  `newgit reference` prints, lists every key in a tracker, resource, or
  export definition, with its default and the template variables each hook
  sees. Run `newgit resource templates --show <name>` to read a starter
  template in full.
- [`trackers.md`](trackers.md) covers the tracker model in depth: root,
  access, mounts and pins, how a workspace history is composed from the
  trackers in Git and split back, roles, binding, merge, and pull.
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
  *Installs*); instances whose identity differs never do. Build output a
  tool refills on its own (`target/`, `.next/cache`) is the same kind of
  state with the rebuild left to the tool: a resource declares it
  `disposable`, with no hook at all, and an eviction drops it (see
  *Stop, start, evict*).

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

The starter templates (`process`, `pnpm`, `install`, `postgres`,
`command-snapshot`, `command-snapshot-migrations`, `compose`, `supabase`,
`external`) are definitions like any other, written to
`.newgit/resources/` and owned by the project from then on. Each states the host tools it assumes, since "ready to adapt" and
"ready to adapt if your database runs on the host" are different claims. A
template may bring the companions it needs, such as the `pnpm-store`
resource it depends on or the `db-snapshots` tracker its checkpoint
deposits into. They are created only when absent, so `resource add` never
overwrites a definition.

The `process` template's port is optional: a process that listens on none of
its own (a worker, a watcher, `supabase functions serve`) deletes `[ports]`,
`[exports]` and `[[render]]`. There is no separate `worker` template, since
it would differ only by those deletions. The `supabase` and `process`
templates carry a commented `workdir` for monorepos (the `supabase` one notes
that `[[render]] path` changes with it). A render `find` that matches 0 times
often means the file lacks that setting, so the tool uses its own default and
every instance shares it; the error says so, and the fix is to add the line,
not to delete the rule.

Four templates give a branch its own database, and they differ in what the
instance owns. `postgres` gives each instance a whole cluster: a supervised
`postgres` process on a claimed port over a data directory in the workspace
(`ownership = "workspace"`), so nothing is shared and teardown is the
workspace going away. `compose` gives each instance its own Compose project
(`COMPOSE_PROJECT_NAME`), with suspend and resume as `stop` and `start` and
cleanup as `down --volumes`. `command-snapshot` and
`command-snapshot-migrations` make a database per instance inside one
server everybody shares, which is lighter but shares that server's roles,
settings and uptime. There is no generic checkpoint for Compose: a stack's
state is in volumes that cannot be copied consistently while it runs, so
`compose` checkpoints through the database service's own dump tool and
says so. Each is added as `database`, so its export is `DATABASE_URL`.

### Committed definitions, local state

`.newgit/` splits into a control plane and a data plane:

```text
.newgit/
  config.toml            [project], and [remote] once linked   committed
  trackers/              tracker definitions                    committed
  resources/             resource definitions                   committed
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
allocated ports, logs, external handles. The content of trackers outside Git stays out of
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

- **Every path has at most one owner.** A tracker is *mounted* at a path
  inside its parent, and claims paths relative to its mount (`!` excludes).
  The longest claim that reaches a path owns it. A claim reaches a path
  under it unless a tracker mounted deeper holds the path and the claim is
  `.`, or a directory around that mount, or the claiming tracker is in Git.
  So only a tracker outside Git reaches into another's mount, with a claim
  inside it: `paths = ["apps/t3code/.env"]` owns that file just as `mount =
  "apps/t3code"`, `paths = [".env"]` would. A tracker's own `!`
  exclusions take it out of the running; identical claims are an error,
  except between role alternatives. `root` owns `.`,
  everything nothing else claims, `.newgit/` included. `access` is mounted
  at `.newgit/trackers/` and holds every tracker's definition, which decides
  who may read what; by default only the project's owner may change it
  (`maintainers = "owner"`). `newgit init` creates both and adopts the
  project's Git history as `root`'s.
- **There is one kind of tracker.** Its `git` key says whether its files are
  committed with Git. It says nothing about sensitivity; that is the
  `audience`.
- **A tracker in Git** (`git = true`) is committed content with its own
  history, stored relative to its own root. Each root commit *pins* the
  commit of every tracker in Git it mounts, and the root's history is the
  workspace's shape: one root commit per view commit. `newgit tracker
  create electron --git --path apps/electron` is one commit that moves the
  files into the new tracker and pins it; history before it stays in root,
  so no id changes. You commit to a tracker in Git with Git. When work
  leaves the machine, newgit splits each commit into one root commit and a
  commit in each tracker it changed, and composing them back gives the same
  commit, with the same id. `newgit pull` moves every pin that is
  behind, at any depth, to the tracker's `follow` ref, after taking what is
  new on its export target when it has one rooted at it, in one commit on
  the branch it runs on (a child's moves by a commit in its parent first);
  `newgit pull t3code ui-kit` only those. No tracker's `main` moves until
  `newgit merge`, and nothing else moves a pin on your behalf.
- **A tracker outside Git** (`git = false`, the default) owns files that
  never belong in Git: an `.env.local`, an agent's notes, a SQLite file, a
  public page. They are captured by newgit, never committed.
- **Binding** decides what an instance works on. Trackers sharing a `role`
  are alternatives (`local-db`, `staging-db`), and an instance binds one per
  role: `spawn --use`/`--drop` first, then your own default in
  `[projects.<project>.roles]` in `~/.newgit/config.toml`, then `priority`.
  A negative priority is never bound unless asked. `newgit tracker use` and
  `drop` change a live instance's binding. A tracker brings resources, and
  they run only while it is bound.
- **Members of a role are forks of each other.** `newgit tracker create
  staging-db --role db --from local-db` starts a tracker outside Git at
  `local-db`'s `main` (two refs in the one local store, nothing copied) and
  records `from = "local-db"`, so the two share history and can merge
  (merging between members is next). A fork's audience is a ceiling: never
  wider than its upstream's, even after `from` is dropped or changed. The
  client refuses a wider one at creation, on every load, and in any commit
  changing definitions; `newgit serve` records what each fork's repo was made
  from and serves a wider one with its upstream's audience. Against
  newgit.dev's Worker only the client checks it. A fork is never detached. A
  tracker in Git cannot be forked yet.

### Trackers outside Git

Capture snapshots the paths of a tracker outside Git into a new revision on the
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

Because a tracker outside Git is pure content, capture and restore need no
modes. If something needs a command to capture or restore, it is a resource.

**Capture never follows a symlink.** A lane holds this workspace's files
and syncs as far as its audience reaches, so a link at a tracked path must
not turn capture into "copy any file I can read into the lane". A link is
judged with `lstat`, never `stat`:

- A link whose target is relative and stays inside the workspace (by its
  text, and resolved on disk) is stored as a link — mode `120000`, the
  target's text as the blob — as Git stores one. A linked directory is kept
  as a link and not walked into.
- A link that leaves the workspace — an absolute target, `..`s that climb
  out, or a path through another link that leads out — is refused, naming
  the path and its target, and nothing is committed. So is a tracked path
  under a symlinked directory: what is "at" it is wherever that link leads.
- Checkout writes a stored link back as a link. A rev holding a link whose
  target leaves the workspace is refused before the workspace is touched,
  whoever wrote the lane: written out, it would point a workspace path at a
  file of the lane author's choosing.

The same applies to every path that reads tracked content: `checkpoint`,
the safety capture before a checkout, dropping a tracker outside Git,
`--from-store`, a resource deposit, install-key hashing (a link hashes as
its target's text), and export (a link is exported as a link).

The content of a tracker outside Git is kept in its own repository, not the
project's. The project repository is what a local `git clone` hardlinks into every
workspace and what a push publishes from; the tracker repository lives
under `local/`, which is gitignored.

### Keeping a tracker's paths out of Git

The files of a tracker outside Git sit inside a Git clone, so an agent could `git add`
them. The rule:

> **A path owned by a tracker outside Git must be ignored by Git.**

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
commit adding or changing a file a tracker outside Git owns is refused, naming the
commit and the file. History from before `newgit init` is exempt, so an old
committed `.env` does not block a first push; removing it is a history
rewrite, which is not newgit's to do silently.

### Audience and storage

Every tracker has an `audience` (`public`, `project-devs`, `user`, or
`user:<login>`) and a `storage` (`local` or `remote`).

Locally, audience decides what `newgit export` ships and nothing else. On
the newgit remote it is enforced: the remote mints tokens per tracker, so a
reader gets only the trackers they may read, and a branch composed for them
holds only those trackers in Git.

`storage = "remote"` makes a tracker outside Git sync. A checkpoint pushes its
revisions to the remote; `spawn --from-checkpoint` fetches them. A tracker
with `audience = "user"` is per user: each person's content is their own
repository on the remote, so spawning a teammate's checkpoint gives you
your own `.env.local`, never theirs. `tracker create --storage remote`
is refused until the project has a remote, rather than quietly behaving as
local.

**A synced tracker outside Git is encrypted to its audience (#338).** With
`storage = "remote"` and an audience other than `public` (no opt-out: what a
server must read, like newgit.dev's `site`, is `public`), each file is
encrypted whole with age on this machine before anything is sent. The
recipients are computed, never read back: the members the audience names, as
the remote lists them now (the remote is the membership authority), by the
`ssh-ed25519` and `ssh-rsa` keys GitHub publishes for each. The ciphertext is
an ordinary blob in the tracker's history, so Git's object model is
untouched; the list is written beside it at `.newgit/recipients` for people
to read, and no code takes a recipient from it. Capture encrypts (reusing a
parent's blob when its content is unchanged and its own age header names
the current keys, exactly, so unchanged content is still no commit),
projection decrypts with the developer's own SSH key (or `$NEWGIT_IDENTITY`),
and the workspace holds plaintext, owner-only. Before a checkpoint syncs or
`tracker merge` or `members add`/`remove` publishes, the member list is read
again and every commit the remote does not have yet is checked against it:
those encrypted to anyone else are made again, on the last commit the remote
has, so nothing encrypted to a member removed since is ever sent. Every send
checks that every file those commits carry is age ciphertext. A workspace
path this machine has held encrypted content at is never captured or sent
unencrypted because a definition (any definition, by any name) now puts it in
a `public` tracker or a tracker in Git, until `newgit tracker allow-plaintext
<path>` on this machine: a confirmation, keyed by path, not a boundary (the
same content copied or moved to another path is not held back). What
counts as already on the remote is kept per remote and project, and
forgotten when the project is linked again. `user` is the login `newgit login` saved, never the remote's say. Publishing `main` fetches the
remote's first and never forces over changes it lacks. Every change to the
recipient set is printed by fingerprint. A tracker that was plaintext is
encrypted on its next capture as a new history whose first commit names the
plaintext rev it replaces; that is the one history replacement. Identities
for sandboxes and cloud agents are #430. The reference's *Encrypted
trackers* has the details.

What it guarantees, and its limits:

- The remote, and anyone who controls its deployment, cannot read what was
  encrypted before it acts: this holds against a remote that only stores
  and serves.
- A remote that acts can read what is captured afterwards. It decides
  membership, so it can add a login whose GitHub keys then become
  recipients, and the only signal is the fingerprints newgit prints
  (`CHANGED`, `+ <login>`). A machine encrypting a tracker for the first
  time has nothing to compare with.
- The remote also serves tracker definitions, and a definition decides
  whether a path is encrypted at all. A machine that has held a path's
  encrypted content (captured it encrypted, or decrypted it into a
  workspace, as a fresh clone's spawn does) refuses, as an error, to
  capture or send that path unencrypted, whatever tracker claims it now,
  until `newgit tracker allow-plaintext <path>` is run there. A machine
  that has never held the content has nothing to compare against and
  captures as the definition says. It is a confirmation step, not a
  boundary: an agent in the workspace can run `allow-plaintext` itself.
  It knows paths, not content: the same content copied or moved to
  another path is not held back.
- GitHub accounts are the key authority.
- There is no integrity: recipients' keys are public, so the remote can
  replace a file with content of its choosing that decrypts cleanly (signed
  tracker commits are [#431](https://github.com/Spheroman/newgit/issues/431)).
- File names, sizes, link targets, commit messages, authors, and dates are
  visible.
- Revs pushed before a tracker was encrypted stay on the remote in
  plaintext: rotate what they held.
- A teammate on an older newgit sees ciphertext, and a capture of theirs
  would push plaintext.

`merge_with_source` is written to every definition but nothing reads it
yet. `newgit merge` merges trackers in Git, and trackers outside Git move only
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
  what they borrow, and newgit never uses them. Checkpoint and undo copy
  their commit between workspace and store with a ref move when the other
  side already holds its whole history, and fetch otherwise
  ([trackers.md](trackers.md), *Copying refs between local repos*).
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
revision, and copies the whole tree with one `clonefile(2)`. An instance that
leaves trackers out is copied from a template of its own binding,
`<workspace root>/.source-template-<key>`, sparse from before its first
checkout: nothing it leaves out is written, so leaving files out makes a spawn
faster, not slower. The same goes for a plain `git clone`, where there is no
template. On a remote, the template also fetches each export target's
branch the workspace will get as `refs/remotes/<target>/<branch>` (a
rooted target's is its tracker's whole history), under
`refs/newgit-template/`. The copy drops those refs but keeps their
objects, so the workspace's own fetch of that branch moves nothing; fetched
into each workspace, t3code's was 17s of a 21s spawn.
`newgit cleanup` removes a template no live binding uses. Then the copy
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

The files of a dropped tracker in Git are left out of the working tree with
Git's sparse checkout. The history still has them, so every instance of a project
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
  1. **refuses what trackers outside Git own**, as above;
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
An export target with `branches = true` (an `export` in a tracker's
definition) adds itself as a remote in each
workspace, and `gh` resolves the one the instance's work belongs to, so `git
push` then `gh pr create` opens a real pull request there (see *Export*).

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
  configured signal (TERM by default) to the group. Its stdin is a pipe that
  never delivers data or end-of-file while it runs, held by the wrapper
  that waits on it (below), so a watcher that quits when stdin closes keeps
  running. No daemon, no restart policy. `autostart = true` has `spawn` start it; the `process` template
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
takes the first of `start`, `start + 100`, `start + 200`, … that no other
instance's record claims and that is free right now. Instances sit 100
apart, not 1, because a claim holds nothing until the instance's server
binds it and only newgit respects it: dev tools that pick an extra port by
scanning up from a default (`remix dev`: 3001–3100) took the claims of
instances packed one apart before those instances bound them. The stride
keeps another instance's claim out of the ports just above each one; the
real fix is the definition declaring and passing every port its tools
open, which the reference tells agents to do. "Free" means it binds
on all of 127.0.0.1, 0.0.0.0, ::1, and ::, with `SO_REUSEADDR` off, so a
Docker container published on a wildcard address cannot hide behind a
loopback-only check (#93).

The binding records are the only ledger. Removing an instance frees its
ports, with nothing separate to drift. A resource's ports are written into
the instance's record before its `prepare` runs, under a short lock held
only while they are chosen, so two spawns installing side by side are never
handed the same port.

A claimed port can still be taken, by another instance's tool or by any
program. So whose a port is gets decided by the process listening on it,
never by the claim: a listener belongs to an instance when it descends from
a process newgit supervises for it, works inside its workspace, or names
its workspace in its command line. `status` (and so the end of `spawn`,
`start` and `rebind`) names the holder of a claimed port that is not the
instance's own, with its pid, directory and instance, instead of leaving a
bind failure in a log, and an instance whose port answers only because
another instance's process holds it is not ready. `newgit ports --check`
(#92) judges every claim the same way: ok when the claimant holds it or
nothing does while its process is down, FAIL when anyone else does, and
undecided for a resource newgit runs no process of, whose port Docker
Desktop's proxy may hold for it. A port an instance's process listens on
that nothing claims is a warning: the tool that will take somebody's claim.
Unrelated listeners on the machine are not judged. If `lsof` is missing or
fails, `--check` says it could not check rather than reporting every port
free.

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

**Several resources may render one file** (#245). An app's `.env.local`
wants its own port, the database's URL, and another service's address, and
each value belongs to the resource it comes from. With one owner per file,
every rule had to live on whichever resource got the file, so `db`'s
definition ended up knowing every app's env files and the apps' definitions
did not mention their own config. Now each resource declares its own
`[[render]]` on the shared path, and the file is the same simultaneous batch
over every resource's rules: all located in the committed content, every
count and the uniqueness of every rendered value checked against the whole
file. Rendering them one after another in dependency order was the other
way to do it, and it would have brought back exactly what simultaneous
replacement removes: a later resource's `find` matching an earlier one's
output, and a file that depends on which resource bound first. So there is
no order, and capture, drift, and undo all work on every claim on the file
at once. Unbinding a resource (`tracker drop`) reverses only its values.
What stays refused is two resources claiming the same text: identical
`find`s or one inside the other at load, and an overlap only the content
reveals at render. Each piece of text has one owner; the other resource
reaches its value through `{{exports.*}}`.

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

**A render never writes through a link** (#283). Before writing any of a
resource's files, it refuses one that is a symlink or lies under a
symlinked directory, naming the path, the link, and its target. Through a
symlinked directory the write would land wherever the link leads: any
directory the user can write, in the workspace or not. Replacing a symlink
at the path would discard it without a word. This is the write side of
[capture's rule](#secret-trackers), judged the same way, with `lstat` and
nothing followed. The bind fails as on any failed render, and a re-render
after `undo`, `tracker pull`, or `tracker checkout` warns. A link that
stays inside the workspace is refused too: it leads to a file other than
the one the committed content describes, which would then carry this
instance's ports into whatever captures or commits it. Taking a render's
values back out, before a capture or when a resource is unbound, leaves a
file reached through a symlink alone. Remove the link, and `newgit
resource rebind` writes the file.

A hard link cannot be told from a regular file, and a write into it would
change every name it has, in the workspace or not. So a render, and taking
its values back out or putting them back after a capture, opens the file
without following a symlink at the path (`O_NOFOLLOW`) and judges the file
it opened, not the path: nothing swapped in after the check is written. A
file with one name is rewritten in place, so a process holding it open, or
a container that bind-mounts it alone, sees the new content. A file with
more than one name is replaced instead, by a temporary file beside the
path renamed over it, as the install store does: its other names keep
what they held, and the path gets a file of its own with the old one's
mode.

**A re-render does not reach a process already running.** A dev server
reads `.env.local` when it starts. When `tracker use` binds a resource whose
render rewrites that file, or a `drop` removes it with its tracker, the
server keeps what it read. newgit cannot know which process reads which
file, so a resource says: `reads = ["apps/admin"]` lists the files and
directories its running process reads, workspace-relative. `use` and `drop`
read the bytes of every rendered path before the change and compare after,
and only a path whose bytes changed counts — a re-render that wrote what was
already there leaves nothing stale and says nothing. For each process that
was running before the change and still is after it:

- its `reads` covers a changed path: it is stopped and started again with
  the action `autostart` names (else its first long-running one), as `undo`
  restarts what it restores. A failed restart is reported, not raised: the
  binding has changed by then.
- its `reads` covers none of them: left alone, silently.
- it declares no `reads`: left running, with a warning naming it and the
  command that restarts it. A guess from `workdir` was considered and not
  taken: a resource run from the workspace root would cover every file, and
  restarting a database because a dev server's `.env.local` changed is worse
  than the warning.

`undo` needs none of this: it stops every running process before it moves
any content and restarts what was running at checkpoint time after the
re-render. `tracker pull` and `tracker checkout` re-render under running
processes too and do not yet say so.

**Instance values must not leak.** A render writes one instance's values
into a path the project owns. Four places they must not reach:

- **`git status` and `git commit`.** Rendered files Git tracks are marked
  `--skip-worktree` in the workspace, so `git add -A` cannot commit this
  instance's port. It is the same job `info/exclude` does for tracker paths.
  While the binding leaves the paths of a tracker in Git out, the workspace is a
  sparse checkout, which works that bit out from its patterns, so they leave
  the rendered files out too, and `sparse.expectFilesOutsideOfPatterns`
  keeps Git from clearing it on a file it finds on disk.
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

  Capture reverses **the spans the render wrote, not every occurrence of
  the value** (#260). A user who types `MIRROR=127.0.0.1:18002` into the
  file has written their own text, and rewriting it would both change their
  data and leave the tracker holding the `find` twice, so no instance could
  render it again. The render is a pure function of committed content, so
  where it wrote each value is recomputable: an occurrence is reversed when
  it sits on a line the render wrote, at the same place in that line. An
  edit on the rendered line itself still reverses when the value occurs
  exactly as often as the render wrote it, since then it can only be one
  thing. Anything else is not guessed. `tracker capture` refuses, naming
  the lines, and it also refuses content that would no longer render.
  `tracker drop` of a tracker outside Git refuses the same way: it takes the
  files out of the workspace, so its capture is the last copy, and the
  file is left as it was to be fixed. The captures newgit takes on its own
  (checkpoint, re-bind, checkout) still record, because refusing a safety
  net loses the work it exists to keep.
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
- **What of the binding can reach the install.** A shared lockfile lists
  every workspace member, bound or not, and the install builds the ones it
  can see. So the key holds which paths the dropped trackers in Git leave out
  of the checkout, and, only when the install is told them through
  `{{bound…}}` (in its own definition, or in any resource's exports or
  renders), the bound tracker names and paths. Binding a tracker outside Git, or
  one with no paths, moves neither, so it shares a tree with the binding
  without it (#248). When a live instance's binding changes (`tracker use`,
  `drop`, or an undo to another binding) in a way that moves an install's
  key, that install is rebuilt for the new key, from the store where
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
  That resource keeps installing per instance. Nothing fails. Generated
  code (a Prisma client) is moved into a resource of its own instead, keyed
  on its own input.
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

#### Every key gets an entry, and a miss costs disk only once (planned, #421)

Two gaps in the store as built, and one rule that closes both: **every key
an instance reaches gets an entry, and every entry shares what it can with
the entries before it.**

**A key that moves inside a live instance stores an entry.** Today an entry
is published only when `prepare` finishes. An agent that runs `npm install
zod` changes the lockfile and the tree, and the store never learns of it:
the next instance at that lockfile runs a full `npm ci`. So a checkpoint
whose instance has moved to a key the store has no entry for publishes the
workspace's tree for it, through the same admission as any other entry.
That tree is whatever the workspace holds, not a fresh `prepare`'s output,
and that is the decision, made loudly: an instance that edited its
`node_modules` by hand publishes the edit. The way back is the one that
already exists: `newgit action <resource>.prepare` runs the command and
replaces the entry, and `undo --force-recompute` drops it.

**A miss is deduplicated against the store.** A miss still runs `prepare`
in full, so `npm ci` stays `npm ci` and its output stays exactly what it
would have been. What changes is what newgit keeps. Most of a new tree is
byte-identical to an entry already in the store: a fresh `npm ci` of
`remote/` matched the existing `node_modules` on 100% of its 290 MB at the
same paths, apart from two binaries an install script swaps in. So after
`prepare` finishes, and before the tree is published, newgit walks it, and
every regular file whose relative path, size, and content hash match a file
in another entry of the same resource is replaced by a copy-on-write clone
of that file. The workspace and the new entry then share those blocks with
every entry before them; only what actually changed takes new disk.

- **Each entry carries a manifest** (path, size, hash) written when it is
  published, so a dedup pass hashes only the new tree, never the store.
  Candidates are found by path and size, and confirmed by hash.
- **The replacement cannot change a byte.** On btrfs and XFS it is
  `FIDEDUPERANGE`, which compares the ranges itself and shares them
  atomically. APFS has no such call, so newgit clones the stored file
  beside the target and renames it over, after checking that the target's
  size and mtime still match what was hashed; a file that changed is
  skipped. Hardlinked files (pnpm's) and symlinks are left alone.
- **It never fails the bind.** It runs after `prepare` succeeded, and a file
  it cannot replace is simply not shared. Where the filesystem cannot clone
  at all, there is nothing to do, and newgit says so once.

Together they make the store a set of independent entries, one per key,
that share blocks as far back as their content agrees. There is no lineage
to track: the filesystem counts references to each block, so dropping an
old entry frees only what nothing else uses. Pruning stays what it is,
entries no live instance keys to.

**Build output is not an install, and this does not reach it.** Measured on
this repository, a workspace's 5.6 GB `target/` is 2.0 GB of integration
test executables, 2.2 GB of debug objects, 2.4 GB of `incremental/`, and
about 0.3 GB of third-party crates. Two workspaces on the same toolchain
share 5% of `target/debug/deps` byte for byte. The rest is the branch's
own code compiled for that branch, which no store, clone, or dedup pass can
share. Its disk comes back by removing instances whose work has landed (see
*Cleanup*), and by building less of it per branch, which is the project's
build configuration, not newgit's.

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

What the project root has and the workspace does not is said, not copied:
a gitignored file there named like a local secret (`.env.local`,
`master.key`) that no tracker brought is named as left behind, with the
three commands that make a tracker carry it (#83). Copying it would make
the root's ignored files an undeclared, unversioned tracker; naming it
turns a service failing hours later into a line at spawn.

Autostart comes after every `prepare`, not interleaved with them, because
`prepare` is a command that finishes and a dependent's `prepare` may need
its dependency's files but never its process. A start that fails is
reported per resource, and a dependent of a resource that did not start is
not started against it. Starting is waiting (#54): a dependent starts once
each dependency is up — its `[ready]` check passes, or with none every
port its process claims answers — given that dependency's `[ready]
timeout_seconds` (five by default), and not at all against a dependency
whose `autostart` action is not running. A process that has been exec'd is not one that serves, and
starting against it only moves the race into the dependent. One withheld
this way stays owed: the next `newgit start` starts it, as it does an
`autostart` action that died. `newgit action` of a long-running action by
hand waits and refuses the same way.

`spawn --from-checkpoint <instance>/<checkpoint>` brings up a checkpoint
as a new instance: its source and uncommitted work, its trackers, and
resources restored from the checkpoint, such as a database loaded from its
dump. Like a bare branch name in Git, the name means this machine's first:
a checkpoint taken here (even of an instance since removed) is used as it
is, with no remote involved. Only one this machine does not have comes from
the remote, which gives the trackers you may read and your own copy of
per-user ones. It is built from parts that already exist: (fetch,) spawn at
the checkpoint's head, then undo to the checkpoint. Its `autostart` actions start after the
undo, since the restore stops and restarts processes to match the
checkpoint. The new instance's workspace comes up on its own branch, made
at the checkpoint's commit, even when the author's was on another branch
or detached (#315).

#### An incomplete spawn must say so

A failed `prepare` does not abort the spawn. A half-bound instance with a
record, a workspace, and logs is more useful than none. But then the
summary can contain a `FAILED` line for a resource that never came up, and
a caller that checks only the exit code cannot tell "ready" from "spawned
but broken". So `spawn` exits 1 whenever any resource did not bind, and its
last line says so:

```text
Spawned branch instance `perf-a` — but `web` did not come up (see below)
  ...
spawned with failures: 1 of 4 resources did not bind.
  `perf-a` exists but is not ready:
  - `web` failed to render: resource `web` renders into `app.conf`, but
    `127.0.0.1:8000` occurs 2 time(s) there, not 1; ... — fix that file's
    committed content or the `[[render]]` in .newgit/resources/web.toml,
    then `newgit resource rebind web`
```

What is under that last line is what `status` says about the instance:
each problem with the error behind it and the command that fixes it. The
fix follows from what failed, and never names an action the resource does
not define (#262). A failed render or export is fixed in the file or the
definition, then bound again with `newgit resource rebind <resource>
[instance]`, which binds it as spawn did (ports, exports, render, `prepare`,
then its `autostart` action) and refuses while its process runs. A rebind
keeps every port the binding holds that the definition still declares and
allocates only one declared since: a port is the instance's for life. A
failed
`prepare` is retried with `newgit action <resource>.prepare`; a resource
with no `prepare` is pointed at the action that builds its tree or its
long-running action, and one with neither (a `[restore]` alone) at its
logs.

An instance bound before its definition changed counts too (#85).
Binding is a moment, not a subscription: an agent that edits
`.newgit/resources/web.toml` while `web` runs changes nothing in an
instance bound before, and would otherwise debug behavior that comes from
the old binding. So `status` holds each ready binding against the
definition as it stands, one line per resource, naming each difference and
the commands that apply it:

```text
  - `web` was bound from an older /repo/.newgit/resources/web.toml: port
    `extra` declared since, so nothing claims it for this instance; export
    `APP_URL` changed; `[[render]]` of other.txt added; `prepare` changed
    since it ran here — apply it with `newgit stop x && newgit resource
    rebind web x && newgit start x`
```

What it compares is what newgit would write now against what the record
says it wrote: the declared ports against the claimed ones, each export
and each `[[render]]` resolved again as a rebind would resolve it, and the
`prepare` that last ran (`prepare_rev` on the binding, the one thing the
record would not otherwise hold). Re-resolving rather than fingerprinting
the file names the very export or file, and also catches a value that moved
because a dependency's did, while a comment changes nothing. A running
process is held against the command its log says it was started with; a
change to that alone is a restart, `newgit stop x && newgit start x`. The
rebind line names the stop and start around the rebind only when the
process runs, since a rebind refuses a running one. What newgit reads each
time it needs it is never flagged: `[ready]`, `depends_on`, `[identity]
paths`, ownership, the checkpoint, restore and cleanup hooks, and a port's
`start`, which only said where its allocation began.

The path in that line is absolute on purpose: definitions are read from the
project root's `.newgit/resources/`, never from a workspace's copy, so an
edit made in a workspace takes effect when it reaches the project root.
Two more cases are named. A resource defined but not bound (added after the
instance bound its resources) is bound by `newgit resource rebind`. A
binding whose definition file was deleted by hand, rather than with `newgit
resource remove`, is named with its process if it runs and the ports it
still claims; `stop` and `remove` stop that process with `TERM`, and
`resource rebind` then releases the binding, taking its values out of the
files it rendered. Its `[cleanup]` went with its definition and cannot run.

A blocked resource counts. Its `prepare` never ran, so it is no more usable
than one whose `prepare` failed; only where the failure started differs.
Every surface (`spawn`, `spawn --json`, `status`, `action`) derives
"blocked" the same way, from the graph at read time, so a script and a
person see one instance (#209). A resource that failed itself is `failed`
even with a dependency down too, because repairing the dependency would not
fix it. An `autostart` action that did not start counts too: an instance
whose dev server is down is not up. The instance is not rolled back.
`--from-checkpoint` judges by the instance the restore leaves (#208).

#### Starting is not failing, until its deadline

A dev server that compiles before it listens (Next.js) is doing what it
should, and a spawn that called it a problem looked like a failure. A running
process whose port is not answering yet is `starting`, a state of its own:
not a problem, not "not ready". After the autostart, `spawn` polls what is
running for up to the longest `timeout_seconds` of what is bound (five
seconds by default) and returns as soon as everything answers, so a fast
server costs nothing. A process with no `[ready]` still unanswered is then
reported as starting, not as a failure, with its log and a pointer to
`newgit status`:

```text
starting: `feature-a`
  - `app` is still starting: not answering yet on web (3000); its log is .newgit/logs/feature-a/app.start-….log
  `newgit status` shows when it is up.
```

`status` shows the same state: the STATUS column says `starting` (a real
problem still says `not ready`, and wins), with the lines under it. It is
checked at read time, so it clears on its own once the port answers.
`spawn --json` lists them under `starting` (`resource`, the `ports` that
have not answered, `check`, and `log`) and exits 0 as for a ready instance.

A port that answers is not always a server that serves: some bind at once
and answer 500 until they have compiled, and a resource newgit does not
supervise (a Supabase stack a `prepare` left running) has no process to
watch at all. A definition's `[ready]` says what up means instead (#276):
a GET that answers 200–399, or a command that exits 0, and
`timeout_seconds` for how long that takes. The end of `spawn`, of
`newgit start` and of `undo` (and so of `spawn --from-checkpoint`) waits
for the longest of those among what is bound, and a dependent's start
waits its dependency's (*Spawn*). With no `[ready]`, up
is the ports answering and the wait is five seconds; a resource with no
process and no check is up when its `prepare` exited 0. Its declared ports
are not checked: they are reservations, and the `supabase` template claims
several a default stack never opens.

Starting has a deadline where one was declared. An agent goes by the exit
code, and exit 0 says the instance works; "starting" forever, with nothing
alive to say it might still come up, was that lie for a stack that had gone
down. So:

- A resource whose definition has a `[ready]` said what up is and how long
  it takes. Past `timeout_seconds` it is not up: a problem, `not ready` in
  `status`, and `spawn`, `start` and `undo` exit 1.
- A resource with no process newgit supervises has nothing alive to tell
  still-starting from gone-down; it has a `[ready]` check, or it is not
  checked at all, so the same holds.
- A supervised process with no `[ready]` has no deadline, because nobody
  declared one, and most dev servers compile for longer than any default:
  it is `starting` for as long as it runs. Its line names its log.

The clock is the process's start (when its pid file was written) or, with
no process, the last `prepare`, `resume` or `restore` that exited 0
(`.newgit/state/<slug>/<resource>.up`), so `status`, which has no clock of
its own, gives the same answer `spawn` did. With no record of either, there
is no grace. A not-up line for a supervised process ends with the last lines
of its run's log: a dev server that crashed under a wrapper that stayed alive
(`npm-run-all`) is a running process, and only its log says why. `spawn`
prints where the instance's logs are (`logs:`; `logs` in `--json`).

`status` runs an instance's checks side by side, one thread each, and
`status <instance>` runs only that instance's. An `https://` check uses this
machine's trust store, so a certificate `mkcert -install` made trusted
passes; one the machine does not trust says so.

#### Died is not stopped

A dev server that crashes, is OOM-killed, or is `kill`ed used to read
`stopped`, exactly like one `newgit stop` took down, and the instance stayed
`ready` on a dead port (#261). The supervisor keeps how a process ended.
Each long-running action runs under a small `sh` wrapper that waits on it;
when the command ends the wrapper appends `[newgit] exited 1` or `[newgit]
killed by SIGKILL` to the action's log and writes the status to
`.newgit/state/<slug>/<resource>.exit`. Before newgit signals a process
(`stop`, a signal action, `remove`, `undo`) it leaves a
`<resource>.stopping` marker, so an ending newgit asked for logs `[newgit]
stopped by newgit` and reads `stopped`.

`status` shows one that ended on its own as `exited(1)`, `killed(SIGKILL)`,
or `crashed` when nothing recorded how (the whole group was killed at once,
or it outlived a reboot). An `autostart` action in that state makes the
instance `not ready`, with `newgit action <resource>.<action>` as the retry,
and `newgit start` starts it again along with what `newgit stop` stopped.
Severity runs ready < starting < not ready: a died process is a problem, a
process that is up but not answering yet is not, until its `[ready]`
deadline has passed. `newgit cleanup` retires a
dead process's pid to `exited`, not `stopped`, so it does not erase a
crash.

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
resource's dependents come back, and with it any `autostart` action that
died on its own since (#261), or that never started though its resource
is ready: one a spawn held back behind a dependency that was not up (#54),
or one an undo stopped and could not start again. `status` shows such a
resource as `not started` and the instance as not ready. A failed restart
is the process's failure, not the binding's: the binding's `failed` says a
`prepare`, restore, export or render did not succeed, and is cleared by
that succeeding, while a process that is not running is cleared by any
start of it that succeeds. A long-running action without `autostart` that nobody started stays
stopped. A resource whose dependency did not come back in the same `start`
is not started, and stays owed, so a dev server is not brought up against a
database that is still down.

The workspace, trackers, installs, and port claims stay, so the instance
comes back on the same ports, unless another program took one meanwhile
(#441). Only newgit respects a claim. So before `start` runs the first
thing it owes a resource (its `resume`, or else its long-running action),
it checks each port the resource claimed, once, and only while the
resource's own process is down: is something listening on it? That is a
bind with `SO_REUSEADDR` on, as real servers bind, on the loopback and
wildcard addresses of both families. A port in TIME_WAIT from the
resource's own server, stopped a moment ago, is free; a Docker wildcard
listener (#93) is not. Checking before the `resume` means the containers a
`resume` brings up on the port are not then taken for another program. A
held port moves to a free one, claimed under the port lock: the binding
record, the exports, and the rendered files all change to it, and `start`
says so (a line on stderr, `moved_ports` in `--json`). One that cannot
move fails the resource, naming the port. A dependent over `depends_on`
that is still running was started with the old port in its environment,
which now reaches the other program, so `start` stops it and starts it
again after the resource, with a warning that says so. A running resource
that only reads the moved exports, with no `depends_on`, is named in a
warning instead.

Both commands do nothing and exit 0 when there is nothing to do, so they
can run on every settle and unsettle. Both exit 1 if any resource failed.

`newgit stop --evict` also frees the instance's disk, for an instance
nobody will touch for a while. After the stop, it checkpoints the instance
(reason `evict`) and saves what neither the checkpoint nor the tracker
store carries. What Git keeps only in the workspace's `.git/` goes into the
store under `refs/newgit/evicted/<slug>/`: local branches, tags, every
stash entry, what is staged, notes (`refs/notes/*`), and the
remote-tracking refs of remotes other than `origin`. The rest goes into
`.newgit/state/<slug>/evicted/`: the workspace's `.git/config`, its hooks
(all but Git's `*.sample` files), its remote token, the mode of every file
and directory whose mode is not what a fresh checkout gives (`0644`, or
`0755` when executable; `0755` for a directory), the empty directories,
and each install store key its trees had. Then it records the checkpoint
in the binding record as `evicted`, moves the workspace aside, writes its
marker back at the path, and deletes what it moved. The marker stays so
that commands run at the path still find the instance, and a tool holding
the path still finds a directory.

`newgit start` restores first. It clones the workspace again beside the
path, as `spawn` does, and moves the clone into the directory that is
there, so a shell sitting in the marker's directory is in the workspace
afterwards, not in a deleted one. The rest runs at the path itself,
because resources are path-dependent (`NEWGIT_WORKSPACE`, a venv, a
rendered `node_modules`). It writes the checkpoint's tracker content, then
undoes to that checkpoint. That brings back the source with its
uncommitted work, tracker content (an encrypted tracker decrypted from this
machine's cache or key), renders, and the trees resources rebuild, filled
from the install store. Then it puts back the refs, the stash, what was
staged, the hooks, and the config. A config key the new clone already has
is newgit's own (its remote, push routing, the sparse checkout) and keeps
newgit's value, except `branch.*`, which is the user's (upstreams,
descriptions); every other key comes back as it was (`user.*`, other
remotes). Last it makes the empty directories again and puts back the
modes, clears `evicted`, and starts what the stop stopped.

The promise is that the `start` gives back exactly what was there, or that
the eviction refuses and deletes nothing. So it refuses, before anything is
stopped, when the workspace holds what would not come back: a merge,
rebase, cherry-pick, revert, or bisect in the middle, or unresolved
conflicts; changes in a dropped tracker's paths (as `undo` refuses); hand
edits to a rendered file; an untracked directory that is a repository of
its own, which `git add -A` records as a commit id; an initialized
submodule, recorded the same way and kept in `.git/modules`; a worktree
linked to the workspace (`git worktree add`), whose Git is in
`.git/worktrees`; and ignored files that no bound tracker outside Git owns
and no bound `recompute` resource rebuilds, such as `target/` or a stray
`.env.local`. It names those, a wholly ignored directory as one entry with
its file count, and `--discard-ignored` evicts anyway, naming them again
(`discarded` in `--json`). Build output a tool refills on its own is
declared `disposable` by a resource
([#447](https://github.com/Spheroman/newgit/issues/447)):

```toml
# .newgit/resources/build-output.toml
ownership = "workspace"
disposable = ["target", ".next/cache"]
```

A plain `--evict` then drops the ignored files under those paths with the
workspace, says so (`disposed` in `--json`, each path with its resource and
file count, and `status` while the instance is evicted), and the `start`
after it leaves them for the build to refill. The refusal names this
declaration. It is a resource, not a project-level `[evict]` list, because
build output is path-dependent state that is recomputed, never copied,
which is what resources are for, and because a resource is bound per
instance: one a tracker brings (`resources = [...]`) stops counting in an
instance that drops that tracker. It has no `prepare`, no identity, and no
hook, so no `undo`, `tracker use`/`drop`, or `spawn` ever touches the tree:
a declaration with no lifecycle holds for every instance the moment it is
written. It only ever drops *ignored* files (anything else is in the
checkpoint), and a file under it that a tracker outside Git owns is kept.
The first way tried, an install-shaped resource (`[identity]` on
`Cargo.lock`, `produces = ["target"]`), had to either empty a live
`target/` from its `prepare` on an `undo` across `Cargo.lock` or fill the
install store with gigabytes of build output. It also refuses
while a process other than itself has its working directory in the
workspace, judged again right before the move: the shell or agent it was
run from included, since the directory would be deleted under it, so it
has to run from outside the workspace. `lsof` (on macOS) runs with `-b`,
so it does not block on a dead network mount, and under a timeout: when it
does not answer, the eviction refuses, deleting nothing. A refusal names
each process by pid, command, and directory. And it refuses when a resource
would not stop, when the checkpoint is degraded, and when the checkpoint
is not whole in the store: every object its source commit and tracker
revs reach, checked object by object past what the store's other refs
already reach (Git keeps those whole). The checkpoint has to be whole on
this machine, not on the remote: only the workspace is deleted. When the
remote did not take it, the eviction says so plainly: the only copy of the
instance's work is then in the project store on this machine.

Not kept, and said before anything is deleted (`not_kept` in `--json`):
reflogs other than the stash's, intent-to-add (`git add -N`) entries,
assume-unchanged or skip-worktree flags set by hand (the content of those
files is kept; only the flag is lost), setuid, setgid, and sticky bits
(the mode's permission bits are kept, and a mode is put back with `lstat`,
never through a symlink), and a value set by hand on a config key newgit
sets itself on restore (`origin`'s URLs and receive-pack, `credential.*`,
its export remotes), worked out by making newgit's own config in a scratch
repository. Of every other config key the clone already has, a
single value comes back as it was and a multi-valued key (an added
`remote.origin.fetch`) gets the saved values added beside newgit's. Evicting does not free the disk an
untracked file's content takes, since its blob is in the store with the
checkpoint, until a restore brings that checkpoint back, a later eviction
supersedes it, and `newgit cleanup` collects it (see *Cleanup*). Each cycle
adds an evict checkpoint and the one or two before-undo checkpoints its
restore's undo takes; a restore that succeeds stamps the evict checkpoint
`restored` and marks those `restore_of` it, for `cleanup`.

A crash or a failure part way loses nothing. A crash after the record says
`evicted` and before the move leaves the original workspace at the path,
with `.git/newgit-evicting` naming the checkpoint: `start` recognises it
and clears `evicted`, and `stop --evict` then evicts again. The move aside
is one rename, so otherwise the path holds its marker or nothing. The
moved-aside workspace (`.<name>.evicting`) holds only what the checkpoint
holds; it is deleted right away, a read-only directory in it made
writable first. If it cannot be, the eviction says so and how, and the next
`start` or `stop --evict` tries again: `stop --evict` before it stops or
records anything, refusing with what to do when it still cannot.

A restore writes `.newgit/state/<slug>/evict-restoring` and keeps
`evicted` until it is done. A restore never deletes what is at the path,
since an editor, a language server, or a shell sitting in the marker's
directory may have written there while it ran. One that fails moves what
is at the path, whole, to `.<name>.restore-failed` beside it, puts the
marker back, and keeps `evicted`, so the next `start` runs it again. One
that is killed part way leaves half a workspace that anyone may have
worked in since: the next `start` moves it, whole, to
`.<name>.interrupted-restore`, and then restores. A second one gets `-2`,
and so on. These are *saved copies*: each carries no instance marker, is recorded in
the instance's binding record (`saved_copies`) before anything moves into
it, and is named, with what to do, on stderr the moment it is made and by
every `start`, `status`, `stop --evict`, and `cleanup` after. `cleanup`
never takes one for an orphan workspace, and `remove` keeps them and names
them. Only `newgit cleanup --discard-saved <path>` deletes one. A restore
refuses when something other than the marker was written at the path since
an eviction or a failed restore, and so does `stop --evict`, rather than
report nothing to do. `remove` refuses too, naming what is there, unless
`--force`. (A `remove` of an instance that is not evicted checkpoints it
first; see *Remove*.) While an
instance is evicted, commands that need its workspace (`checkpoint`,
`undo`, `run`, `merge`, ...) say it was evicted and to run `newgit start`.
A second `stop --evict` says it is already evicted to its checkpoint.
`status` shows it as `evicted`, `cleanup` does not treat it as an instance
whose workspace is gone and keeps the install store entries its restore
refills from, and `remove` deletes what the eviction kept.

`suspend` never runs for a `project` or `user` resource, which other
instances share, nor for one that never came up. The `supabase` template's
pair is `supabase stop` / `supabase start`, which keeps the data.

A tracker drop is the same stop, for the resources the dropped tracker
brought (#254). Each is *parked*: taken down — its process signalled and
its `suspend` run, or its `[cleanup]` when it has no `suspend` — and its
binding moved to the record's `parked` table with its ports still claimed.
`tracker use` hands those ports back and runs `resume` (or `prepare`, after
a cleanup), so a role switch and back leaves the rendered files, the data,
and the running stack on the same ports (#255). `newgit ports` lists parked
claims as `(parked)`, and `ports --check` counts them as claimed. An undo to
a checkpoint that bound less parks the same way.

### Remove

`newgit remove <name>` checkpoints the workspace (reason `remove`), stops
what is running (dependents first), runs `[cleanup]` hooks where ownership
allows — for parked bindings too, unless their drop already ran it —
deletes the workspace (`rm -rf`; clones have no registration), and
archives the binding record, which names the checkpoint (`removed_to`).

**Remove never deletes work no checkpoint holds**
([#448](https://github.com/Spheroman/newgit/issues/448)). Before anything
is stopped, it takes a checkpoint as `stop --evict` does: uncommitted,
untracked, and staged work, the workspace's commits (which the store then
holds, so the branch rule below sees them), tracker content, and resource
state, captured while the resources still run. `newgit spawn <name>
--from-checkpoint <name>/<id>` brings the workspace back. "Remove, and I
can undo it" is the obvious reading of a remove that keeps checkpoints,
and refusing a dirty workspace instead would make every unattended caller
(T3 Code deletes a thread with a plain `newgit remove`) either fail or
learn `--force` and lose the work anyway. The checkpoint neither syncs to
the remote, which the instance leaves a moment later, nor re-encrypts an
encrypted tracker's rev to a changed audience, so a removal never waits on
the network.

An evicted instance is not checkpointed again: the checkpoint it was
evicted to holds its workspace, and is the one the archived record names
(`removed_to`) and `remove` reports. That is T3 Code's most common path, a
thread whose workspace was evicted at the end of a turn and then deleted.

What a checkpoint cannot hold is named as lost (`lost` in `--json`), never
deleted in silence: ignored files no tracker outside Git owns and no
resource rebuilds (`target/`, with its file count), changes in a dropped
tracker's paths, hand edits to rendered files, a merge or rebase in
progress, the stash, branches and tags only the workspace has, staged
content the working tree has moved past, a Git repository nested in the
workspace (a checkpoint records only the commit it has checked out, so its
files go; `lost` says how many uncommitted changes and commits on no
remote it had). Ignored paths a resource declares `disposable` are listed
as dropped, not lost.

A resource capture that fails (a `pg_dump` against a database that is
down) does not refuse the removal when the resource's state is outside
the workspace: the checkpoint goes ahead without that resource, as it
does for a resource that is not up. But its state is then in no
checkpoint, so its `[cleanup]` does not run: a `dropdb` after a failed
dump would delete the only copy, unattended. The state stays where it
is, and the removal says so (`left:`, `left` in `--json`, hook status
`skipped-uncaptured`), with the cleanup command as it would have run,
which names where the state is and removes it by hand. `--force` runs it,
and names what that deleted as lost.

When the resource's state is in the workspace, it cannot stay where it
is: the workspace is what the removal deletes. A Postgres cluster whose
data directory is in the tree is the case. So that failed capture
refuses the removal, deleting nothing (the checkpoint it took holds the
rest), unless `--force`, which names the state as lost. Where the state
is comes from the definition, `ownership = "workspace"`: newgit cannot
see where a command keeps what it captures, and the ignored files the
removal deletes cannot be told apart from build output, so a guess from
them would either refuse every removal with a `build/` in it or let the
database go. A resource that keeps its data in the tree but declares
another ownership is taken at its word.

A checkpoint that fails refuses the removal, deleting nothing, unless
`--force`, which removes anyway. So does a nested repository with no
commit, which `git add` refuses, and a workspace whose Git cannot be read;
each refusal names `--force`. `--purge` takes no checkpoint (it discards
them all). With no checkpoint (`--purge`, or `--force` past a failed one)
`lost` names the uncommitted work it deletes and, per bound tracker
outside Git, the paths whose content differs from its bound rev (compared
without capturing). Under `--force` or `--purge` the analysis is
best-effort: what could not be read is named (`could not inspect ...;
whatever it held`) rather than refusing.

A removal that fails after its checkpoint leaves that checkpoint; the
retry takes another, and `cleanup` collects the earlier one. A checkpoint
that fails partway deletes the store ref it wrote.

Every instance spawned under a name shares its checkpoint history, so
each checkpoint record names the instance that took it (`instance`, the
instance id). `cleanup` collects a `remove` checkpoint only when the same
instance took a later one: a later instance's removal is its own work,
not a retry. Archived records are never replaced: each claims its name
(`<slug>-<timestamp>.toml`, `.<n>` added when an earlier removal took it
in the same second) before the record is moved onto it.

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

With no checkpoint first (`--purge`, or `--force` past a failed one),
when no ref holds the workspace's `HEAD`, it says the commits since the
last checkpoint are lost, so "nothing unique on the branch" never reads as
"nothing lost".

**Checkpoints outlive removal.** They pin what their undo would need, and
newgit never breaks an undo on its own initiative. `remove` reports how
many it kept; `--purge` discards them: the ones this instance took, never
an earlier instance's under the same name, nor one an archived record
names (`removed_to`, `evicted`). `cleanup --purge-archived` discards a
name's whole history once no live instance has it.

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
  unreachable. It never deletes a revision a checkpoint points at, nor a
  checkpoint record beyond what retention collects (below), and reports how
  many it kept for that reason, so retained disk is explained.
- **prunes the install store** (see *Installs*).
- **collects what eviction leaves behind**
  ([#446](https://github.com/Spheroman/newgit/issues/446)), only what is
  proven to be nobody's only copy. History is kept by slug and outlives an
  archived record, so a later instance with the same name shares it. In
  each slug's history, live or archived, it collects an `evict`
  checkpoint a restore brought back and that succeeded (`restored`), once
  a later eviction supersedes it; every checkpoint a restore that
  succeeded took on its way (`restore_of`: the marker and a clone); and a
  `remove` checkpoint the same instance took a later one after (a removal
  that failed after its checkpoint, retried). It never collects an `evict` checkpoint nothing
  restored, since an instance removed while evicted keeps its work only
  there. Nor does it collect one any live or archived record names
  (`evicted`, `removed_to`), a before-undo that a person's own `undo` took
  (its redo point), one a restore that failed took (it may hold what was
  written at the path meanwhile), or an `explicit` or `push` checkpoint. It
  also collects checkpoint refs no record names (a checkpoint killed
  between writing its ref and its record). It deletes each record and the
  store ref of its source, so the tracker revisions only they held are
  pruned in the same pass. A collected id is never handed out again. It
  reports the bytes of store objects only they reached (`git rev-list
  --disk-usage`) as now unreachable. **newgit does not prune the store**:
  the publish outbox (`.newgit/local/publish/outbox.git`) borrows the
  store's objects through alternates, so a prune of the store can corrupt
  it, and the user's own `gc.pruneExpire`, `gc.auto`, and `git maintenance`
  decide when unreachable objects go (by default two weeks, which is also
  how long `git fsck` can recover a dropped stash). Each slug's lock is held
  only while its records are read or deleted. An evict checkpoint is not
  always "nobody's work": in T3 Code it is often the only checkpoint of a
  turn's end state. That is why one is collected only after a restore
  proved the workspace holds it again and a later eviction moved past it.
  **A collected checkpoint is gone for `undo --to` too**: that is the
  decision, since each one is a state the instance was in only on its way
  somewhere else. A state worth keeping is kept with `newgit checkpoint`,
  which nothing collects. The copies a checkpoint sync sent to the remote
  are not touched.

That conservatism is a dead end for archived instances: `undo` needs a
binding record, so their checkpoints come back only through `spawn
--from-checkpoint`, yet they pin revisions forever. `cleanup --purge-archived` discards them, with their
checkpoint refs. It is opt-in and never touches a live instance. The
ordinary report says how many retained revisions it would free.

**Landed instances are most of the disk, and cleanup should name them
(planned, #422).** Workspaces are disposable, but nothing disposes of
them: instances accumulate after their work merges, each holding a full
build tree. On this project's own machine, 90 of 122 instances held 321 GB
of `target/` alone, with the disk 98% full; removing the 61 whose work had
landed freed 150 GB. An instance has **landed** when its workspace has
nothing uncommitted and its tip is on the project's default branch or on a
branch the remote has, or its branch's pull request merged. `cleanup`
lists landed instances with what each holds on disk, and `cleanup
--landed` removes them as `remove` would: branch kept when unique,
checkpoints kept. It stays opt-in, because newgit cannot see what else is
attached to a workspace: a T3 Code thread or an editor still pointed at
it.

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
- **Trackers**: a revision of every bound tracker outside Git (identical content
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

### A workspace off its instance's branch

Agents run `git checkout -b try-x` inside a workspace as a matter of
course. A checkpoint holds the workspace as it stands, and which branch is
checked out is part of that: the record keeps it (`checked_out`, or
`detached` for a detached `HEAD`, mid-rebase included). The instance's
branch in the project repository moves only when the workspace is on it.
Moving `feature-a` to `try-x`'s commits would be a merge nobody asked for,
so newgit leaves `feature-a` where it was and says so, naming both
branches. Refusing instead would block the checkpoint, the push, and the
undo exactly when an agent is experimenting, and those are the moments a
checkpoint matters most (#259).

Undo puts `HEAD` back where the checkpoint had it: on its branch at its
commit, or detached. It moves only that branch, never the one the workspace
happens to be on. When the branch it moves is not the current one and holds
commits the checkpoint does not, undo prints the old tip. The instance's
branch follows only a checkpoint taken on it.

`merge` and a push to a newgit remote carry what the checkpoint holds,
which is whatever the workspace has checked out. Both repeat the
checkpoint's warning. `status` shows the divergence:
`` feature-a@1a2b3c4d (workspace on `try-x`) ``, and `checkpoints` marks each
checkpoint taken off the branch with `` [on `try-x`] ``.

A resource's restore is one of `none`, `command`, `recompute` (re-run an
action, `prepare` by default, when the identity changed), or `external`.
`recompute` is correctness for path-dependent artifacts, not an
optimization, and it fills from the install store when it can. A tree
recomputed from its inputs holds nothing a dependent generated into it
(`prisma generate` into `node_modules`), so once an undo recomputes a
resource, every `recompute` dependent after it recomputes too, whatever its
own identity says.

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

What an undo leaves when a restore fails is deliberate: that resource
down, and everything that depends on it down too. A server started over
state its restore left half done serves it as if it were fine, and a
dependent started against it reads as up while it serves the wrong data.
The binding records why (`restore_error`), so `status` says `failed to
restore` and names the retry, another undo, rather than the action that
starts the process. The next undo that succeeds brings them all back.

A restore runs with its own process stopped (every process is stopped
before content moves) and its dependencies restored and, when they were
running, started again; a restore that runs something waits for those to
be up, as a dependent's process does. A restore that needs its own server
starts one around the load: the `postgres` template loads the dump into a
fresh cluster under a temporary server on the instance's port and swaps it
in only once the load succeeded. newgit offers no "restore with the process
up": loading into the live server is what forces a restore to reset the
database first, and a reset followed by a failed load is the half-done
state above.

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
  `HEAD`. That includes the files of every tracker in Git, whatever its audience:
  this export filters only trackers outside Git.
- **Trackers outside Git are filtered by audience, failing closed.** Only
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
all, to an ordinary repository a tracker's definition declares with
`export`, typically a GitHub mirror. Where something ships is said by the
tracker itself:

```toml
# root.toml: the project's mirror
export = { url = "github.com/acme/widget", branches = true }
# a carved child: wherever its parent ships (`tracker create --git --path`
# writes this)
export = "inherit"
# a library inside the mirror and on its own, rooted at itself
export = ["inherit", "github.com/acme/lib"]
# no `export`: it never ships
```

A target ships the tracker declaring it and every tracker below that
inherits its export, so what leaves fails closed. A target declared by a
tracker other than root is rooted at it, its files at the top. Definitions
live in `access`, so widening where something ships takes an owner's merge.
An inheriting tracker whose audience is narrower than the declaring one's
is refused rather than sent past it, and a commit the target does not have
yet that adds a file a tracker outside Git owns is refused: what the target
has, asked of it each time, is published already and is not judged again
(#444). It
pushes with your own Git credentials, so GitHub Actions and `gh` keep
working. When the target's branch has moved ahead (a pull request merged on
GitHub), export brings that in first. A target force-pushed to purge
something is not merged with: plain `pull --remote` and export refuse and
name `newgit pull --remote <target> --force`, which resets the rooted
tracker's line to the target's, lists what it drops and who still holds
it, and refuses to carry it onward from this machine; the project's own
history still pins it ([trackers.md](trackers.md), *Following a
force-push*). Every push to a target from a client, of `main`, of an
instance's branch, or of a tag, passes that check (the remote's own export
of its GitHub mirrors does not yet, [serve.md](serve.md)), but it reads this machine's record of what
it last exported there: with no record, or a record older than another
machine's export, a rewrite cannot be told from history of the target's
own (#473). With `branches = true`, every push from a workspace exports its
branch there too: the target's `main` plus the instance's own commits,
whichever was exported first. `newgit export <instance> --remote <name>`
exports that branch again as last pushed, for an export that failed, or
with `--force` to replace it whatever it holds. A branch whose commits
there were built on a copy of what the target's `main` holds is replaced,
only while the target holds exactly what newgit last pushed there by its
own record, so a commit someone else pushed is never thrown away
([`trackers.md`](trackers.md), *One commit, one root commit*).

### The remote

The newgit remote, at [newgit.dev](https://newgit.dev) or self-hosted from
`remote/`, is how an instance travels. It is a Cloudflare Worker holding one
Git repository per tracker. It gives you:

- GitHub login, project members, and a recorded, revocable token for every
  login, agent, and instance.
- Audience enforced server-side: tokens are minted per tracker, so a reader
  can read only what they may, and `git clone` of the endpoint composes a
  branch from the trackers in Git you may read.
- Checkpoints that sync, and `spawn --from-checkpoint` to bring one up
  anywhere, with your own copy of per-user trackers.
- `newgit merge [instance] [--into <branch>]`, which makes the merge
  commit in your view and commits it at root's ref by compare-and-swap, so
  every tracker moves together or none does.
- `spawn --private`, an embargo: only you see the instance until `merge`
  publishes it.

[`remote.md`](remote.md) has the design and its status.

---

## Known gaps

Open issues that change what this document can promise:

- **#54: `newgit action app.start` by hand does not start what it needs.**
  It refuses while a dependency is down, naming it, rather than starting
  it as `docker compose up app` would.
- **#64: rendered files are hidden, not filtered.** `--skip-worktree` makes
  hand edits to a rendered file die with the workspace. A clean/smudge
  filter would make them ordinary, committable edits.
- **#83: gitignored secrets reach a workspace only through a tracker.**
  `spawn` names the ones it left behind, by file name, and the commands
  that carry them; nothing carries them by itself.
- **#85: reconcile is asked for, not automatic.** A live instance does not
  pick up a changed resource definition by itself: `status` names each
  difference and the commands that apply it, and the agent runs them. A
  rebind does not rebind what depends on the resource (#289): `status` then
  names a dependent's export or render that moved with it, but not a
  process started with an environment that has since changed (another
  resource's export, a port's `env` name).
- **#87: preflight is asked for, not automatic.** `newgit resource check`
  finds a definition calling a missing binary (`pg_dump` on a host without
  Postgres); nothing runs it at spawn, so one never checked still fails at
  the first checkpoint or, worse, the first restore.
- **#89: a failed `prepare` can destroy its own evidence**, when the tool
  tears down its containers before exiting. newgit destroys none of it
  itself: every run has its own log, and nothing it runs after a failure
  removes what the command left. It shows the whole command that ran and
  how its log ends (stderr only when `captures` is set), and the reference
  says how to rerun the tool by hand with its own keep-it-up flag; it
  cannot keep the containers itself, and there is no generic
  `--keep-failed`, since newgit runs nothing after a failure for one to
  switch off.
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
