# The newgit guide

How newgit works and how to drive it, in more depth than the
[README](../README.md). For every tracker and resource key, its default, and
which template variables each hook sees, run `newgit reference`: the
definition format travels with the binary. The design documents,
[`newgit-v1-mvp.md`](../newgit-v1-mvp.md) and
[`newgit-architecture.md`](../newgit-architecture.md), explain why it is
shaped the way it is.

The central idea shows up in the code shape: branch instances bind source
state to user-defined trackers and resources, instead of baking env files,
installs, processes, databases, or external resources into special internal
lanes.

## The workflow

All eight v1 milestones are implemented. The workflow the MVP set out to make
feel normal now runs end to end:

```sh
newgit init
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit tracker capture runtime-env --from-store   # seed the lane from the file you already have
newgit resource add deps --template pnpm
newgit resource add app --template process

newgit spawn auth-refactor
newgit action app.start auth-refactor
newgit checkpoint auth-refactor -m "before agent"
# an agent works in the branch workspace, and you dislike the result
newgit undo auth-refactor
# ...or you like it: from the workspace, a plain `git push` checkpoints and
# publishes to the project's real remote

newgit export auth-refactor --to ../public-export
newgit cleanup
```

With a remote (the hosted one, `https://newgit.dev`, is invite-only for
now; the Worker in `remote/` deploys your own), the same checkpoint
travels: a teammate, or a cloud agent's checkpoint, comes up on your machine
with its source, uncommitted work, your own copy of its trackers, and its
resources restored.

```sh
newgit login                      # GitHub device flow
newgit remote add                 # create <you>/<project>: a repo per code tracker
newgit members add octocat
newgit spawn --from-checkpoint auth-refactor/ckpt_003
newgit merge auth-refactor        # into main, tracker by tracker, all at once

# a teammate
newgit clone jack/widget          # or plain: git clone https://newgit.dev/jack/widget.git
```

Plain `git clone` works too: the remote composes each branch from the code
trackers you may read, so two people can read different parts of one
project.

Every tracker is its own history. `root` owns `.` — whatever no more specific
tracker claims — and starts as your project's Git history; `project` owns
`.newgit` (resources, scripts, config), and `access` owns `.newgit/trackers/`,
who may read what, which only the project's owner changes by default
(`maintainers = "owner"`). Carve out a code tracker, history included, with
`newgit tracker create electron --code --path apps/electron`. Trackers are
secret by default: captured, never committed.

An instance works on what it binds:

```sh
newgit spawn fix-db --use staging-db --drop electron   # one per role; the rest as you say
newgit tracker drop electron --stash   # out of the working tree, history untouched
newgit tracker use electron            # back, with the stashed work
newgit status                          # ready / not ready, and why
```

Trackers sharing a `role` are alternatives (`dev-db`, `staging-db`); a spawn
picks one by `--use`, then your own default in `~/.newgit/config.toml`, then
`priority`. A tracker can bring resources (they run only while it is bound) and
`require` others.

## Adopting it in a project

```sh
cd your-project        # any Git repository with at least one commit
newgit init
```

`init` prints what to commit. Run it again where `.newgit/` already exists
(a plain `git clone` of a newgit project, or one set up by an older newgit)
and it adopts what is there: it writes any missing built-in tracker
definitions and gives `root` the project's history. The split matters: `.newgit/config.toml`,
`trackers/`, `resources/`, and `scripts/` are the control plane and belong in
Git, so teammates and CI see the same orchestration. Everything else under
`.newgit/` — branch bindings, captured content, checkpoints, logs, runtime
state — is local and is gitignored for you.

Scripts your resources shell out to go in `.newgit/scripts/` and are
referenced as `{{scripts}}/<name>`. They resolve from the store, not from the
workspace, which is the same rule the resource definitions follow — so you can
edit a `prepare` script and re-run `newgit action` without committing the
attempt first. A script your *project* owns still lives in the project tree
and must be committed before a spawn that calls it.

Nothing about your repository changes until you ask for it. `init` writes
`.newgit/`, and `tracker track` appends to `.gitignore`; no command rewrites
source history, and the one branch newgit ever deletes is an instance's own:
`remove` deletes the source branch `spawn` created for it, and only when its
tip is already on another branch, a tag, or the remote (asked live; an
unreachable remote keeps the branch). Otherwise it is
kept, and `remove` says why (`--keep-branch` / `--delete-branch` override).

### What your package manager will cost you

Every branch instance gets its own installed dependencies. That is what makes
instances independent — two branches with different lockfiles must not share
a dependency tree, or one branch's install silently rewrites the other's —
and it is why installs are resources rather than trackers.

How much that costs is set by your package manager, not by newgit. **pnpm**
hardlinks packages from one shared store into each tree, so a tenth instance
adds directory entries rather than gigabytes; Yarn PnP skips the tree
entirely, and `uv` does the same for Python. **npm** expands a full copy per
instance — its cache holds tarballs, so `npm ci` re-expands every time, and
ten instances of a monorepo means ten full copies of `node_modules`. `pip`
into a per-instance venv behaves the same way.

This is the one place where newgit multiplies a cost you already had instead
of absorbing it, and there is no lever on newgit's side: a shared installed
tree is the thing that would be wrong. If you run many instances of a large
repository, a package manager with a content-addressed store is worth more
here than it is on plain Git.

## Using it with T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) gives each agent thread its
own Git worktree. A fork,
[`Spheroman/t3code`](https://github.com/Spheroman/t3code), gives it a
newgit instance instead. In a project with `.newgit/config.toml` at its
root, "New worktree" runs `newgit spawn <branch> --from <base> --json` and
opens the thread in that workspace, with its trackers bound and its
resources prepared. Deleting the thread runs `newgit remove`. Every other
project gets plain worktrees, as before.

Settling a thread does not touch its instance yet. Running resources stay up
until the thread is deleted, which in T3 Code is rare. The plan
([Spheroman/t3code#2](https://github.com/Spheroman/t3code/issues/2), which
needs an instance-wide `newgit stop` / `newgit start`,
[#157](https://github.com/Spheroman/newgit/issues/157)) is to stop resources
on settle, start them again on unsettle or a new message, and keep `remove`
for delete. Until then, `newgit action <resource>.stop <instance>` releases
one by hand.

On macOS it installs as "T3 Code (newgit)", beside upstream T3 Code: its
own app ID, state in `~/.t3-newgit`, and no auto-update.

```sh
git clone https://github.com/Spheroman/t3code && cd t3code
scripts/fork/install-mac.sh
```

- `T3CODE_NEWGIT=0` turns it off; `T3CODE_NEWGIT_BIN` names a `newgit`
  that is not on the T3 Code server's `PATH`. If `newgit` cannot run, the
  thread falls back to a Git worktree and the server logs why.
- Threads keep their temporary `t3code/<hex>` branch name. T3 Code renames
  a worktree's branch after the first message, and an instance cannot be
  renamed.
- T3 Code's storage cleanup leaves newgit workspaces alone; `newgit cleanup`
  owns them.
- An agent in a thread pushes and opens pull requests the usual way, `git
  push` then `gh pr create`. On a newgit remote, `origin` is the newgit
  endpoint, which `gh` cannot read, so give the project an export target
  with `branches = true` (`newgit reference export`): each push then lands
  the branch there too, and the workspace's remote for it is the one `gh`
  uses.

The fork is a small patch stack rebased onto a fast-moving upstream; its
[`FORK.md`](https://github.com/Spheroman/t3code/blob/newgit/FORK.md) lists
every change, its limits, and how to rebase.

## Using it with Claude Code

Claude Code makes a Git worktree for `claude --worktree <name>`, for a
subagent with `isolation: "worktree"`, and for each background session.
In a newgit project, one command makes each of those a newgit instance,
with its trackers bound and its resources prepared:

```sh
newgit claude-code install            # writes .claude/settings.local.json
claude --worktree auth-refactor       # spawns instance `auth-refactor`
```

`install` adds Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks:
the first runs `newgit spawn <name>` and hands Claude Code the workspace,
the second runs `newgit remove` when Claude Code removes the worktree.
`--shared` writes `.claude/settings.json` instead, for a team that all has
`newgit` on its `PATH`.

- Claude Code runs a project's hooks only once you have trusted the
  folder: run `claude` there once and accept the prompt before using
  `--worktree`.
- `claude --worktree <name>` with a name already in use resumes that
  instance, as it would a Git worktree.
- Each workspace gets the two hooks in its own
  `.claude/settings.local.json`, kept out of Git through its
  `.git/info/exclude`. Claude Code reads the remove hook from the
  directory the session ends in, so without it, choosing "Remove worktree"
  on exit would leave the instance behind.
- A hook replaces Claude Code's worktree handling entirely, so
  `.worktreeinclude` is not read. Trackers are how files Git shouldn't hold
  reach the workspace.
- Claude Code keeps a subagent's worktree when the subagent ends and never
  runs the remove hook for it. `newgit status` lists those instances (named
  `agent-<id>`); `newgit remove` them when you're done.
- `claude rm` on a background session refuses while it has unpushed
  commits, and a workspace's commits stay in its own clone until pushed.
  Discarding them there discards them for good.
- `install` refuses when the settings file already has a `WorktreeCreate`
  hook, since only one hook can make the worktree.
- A spawn whose resources did not all come up still hands Claude Code the
  workspace, and says so on stderr; `newgit status <name>` shows which.

## Commands

Branch-instance lifecycle (top-level verbs; `[instance]` is inferred when run
inside a workspace):

- `newgit init` / `spawn <name> [--use <tracker>] [--drop <tracker>] [--private]` /
  `status [name]` / `remove <name> [--purge] [--keep-branch | --delete-branch]` —
  `status` says whether each instance is ready, and why not
- `newgit status <instance> --path` — just the workspace path, for scripts
- `newgit reference` — the definition format, every key and default
- `newgit run [instance] -- <command>` — run with exports and ports loaded
- `newgit env [instance]` — the environment `run` and every hook get, each
  variable labeled with the declaration it came from
- `newgit action <resource>.<action> [instance] [--dry-run]` — `--dry-run`
  prints the rendered command and its directory, and runs nothing
- `newgit checkpoint [instance] [-m <msg>]` / `undo [instance] [--to <id>]` /
  `checkpoints [instance]`
- `newgit export [instance] --to <dir> [--include <path>] [--exclude <path>]`
- `newgit export --remote <name> [--dry-run] [--force]` — push the trackers'
  `main`s, history and all, to the target in `.newgit/exports/<name>.toml`
  (a GitHub mirror, say); `public` trackers plus the ones it names
- `newgit cleanup [--dry-run] [--purge-archived]`
- `newgit ports [--check]` — claimed ports, or detect a listening one nothing
  claims

Definition management (noun subcommands):

- `newgit tracker create <name> [--audience <a>] [--storage <s>]
  [--merge-with-source]`
- `newgit tracker create <name> --code --path <path>...` — a committed code
  tracker, carved with its history out of whichever tracker owns the paths
- `newgit tracker track <tracker> <path>...`
- `newgit tracker capture|merge|pull|checkout <tracker> [instance]`
- `newgit tracker capture <tracker> --from-store` — seed a lane from the store
  repo's working tree, so the first `spawn` comes up with the content
- `newgit tracker list` — a tree by ownership: kind, role, priority,
  maintainers, and (inside a workspace) what is bound
- `newgit tracker use <tracker> [instance]` / `newgit tracker drop <tracker>
  [instance] [--stash | --discard]`
- `newgit log <tracker> [instance] [-- <git log args>]` — one tracker's
  history: an instance's branch as of its last checkpoint, or the project
  root's
- `newgit resource add <name> --template <template>` / `list` / `templates`

Remote:

- `newgit login` / `logout` / `whoami`
- `newgit remote add [--url <url>] [--project <owner>/<name>]` / `remote show`
- `newgit remote delete <owner>/<name>` — the owner deletes the project
  from the remote: every repo, record, and token pinned to it. Not undoable
- `newgit clone <owner>/<name> [dir]`
- `newgit merge [instance] [--into <branch>]` — `newgit merge main` merges
  the project root
- `newgit members add|remove <github-login>` / `members list`
- `newgit spawn <name> --from-checkpoint <instance>/<checkpoint>`

Resource templates: `process`, `pnpm`, `install`, `command-snapshot`,
`command-snapshot-migrations`, `supabase`, `external`. `newgit resource
templates --show <name>` prints one in full without creating anything.

## Behavior worth knowing

**`export` fails closed.** Source ships (its audience is everyone) and so do
trackers whose audience is `public`. Anything narrower is withheld and
reported; `--include <path>` overrides. It writes one commit, not history,
because exporting the branch's commits would carry any file they contain —
including withheld ones. This is a path-level filter, not concealment.

**GitHub stays where it is.** `newgit export --remote github` pushes a
composed history to an ordinary repository defined in
`.newgit/exports/github.toml` (`url = "acme/widget"`, `trackers =
["root"]`). Only `public` trackers and the ones a target names ship, so a
public mirror and a private one can show different views, and a history that
ever committed a secret tracker's file is refused. Exporting again
fast-forwards, after bringing in whatever was merged on the target since,
and the push uses your own Git credentials, so `gh`, GitHub
Actions, and anyone cloning from GitHub keep working. With `branches =
true`, every push from a workspace exports its branch there too, and on a
newgit remote the workspace gets the target as the remote `gh` resolves, so
an agent's `git push` then `gh pr create` opens a real pull request.

**`[[render]]` puts this instance's ports in the config file your tool
actually reads.** Most tools take a port from a committed config file rather
than argv, so a resource declares literal substitutions into one:

```toml
[[render]]
path = "supabase/config.toml"
replace = [
  { find = "port = 54321", with = "port = {{ports.api}}" },
]
```

There is no template file — `port = 54321` is your working default, so a
clone without newgit still starts on it. `find` is literal, never a regex,
and must match exactly once; that check is also the drift detector, failing
the bind by name when the default changes upstream rather than quietly doing
nothing. Rendered source files are marked `--skip-worktree`, so they never
show in `git status` and `git add -A` cannot commit them — which also means
**real edits to a rendered file do not survive the workspace.** newgit says
so at bind. For a tracker-owned file, `tracker capture` reverses the
substitution, so edits you make beside the rendered value reach the lane and
your port does not.

**A workspace's `origin` is the real remote.** `git pull` and `gh` see the
project's repository, and `git push` checkpoints the instance before
forwarding the push there — succeeding only if that remote accepted it.
`gh pr create` works from the workspace with no flags.

**`cleanup` never breaks an undo.** It finalizes instances whose workspace is
gone, deletes unclaimed workspaces and dead process state, and prunes tracker
snapshot revs nothing references — but never a rev a checkpoint still points
at, and never a checkpoint record. `project`- and `user`-owned resources are
never torn down by per-branch cleanup.

The one exception is one you ask for. An archived instance's checkpoints are
unreachable — `undo` needs a binding record — yet they go on pinning revs, so
`newgit remove <name> --purge` and `newgit cleanup --purge-archived` discard
that history and release what it held. Live instances are never affected, and
ordinary `cleanup` says how many of its retained revs the flag would free.

## Development

This repository uses [`mise`](https://mise.jdx.dev/) to pin Rust.

```sh
mise install
mise run ci        # fmt-check + clippy -D warnings + test, exactly what CI runs
mise run test
mise run install   # build and install into ~/.cargo/bin
```

CI runs the suite on Linux and macOS, checks the declared MSRV, and smoke
tests the release binary against a throwaway project — the tests here drive
real `git`, real process groups, and real ports, so they are integration
tests by nature.

## Workspace Layout

- `crates/newgit-core` holds the domain model: branch instances, tracker
  definitions and their histories, the compose/split engine that turns
  tracker histories into one workspace history and back, resource
  definitions and lifecycle hooks, metadata storage, real-directory
  materialization, checkpoints and undo, export, cleanup, and the remote
  client.
- `crates/newgit` is the CLI, published as the `newgit` crate.
- `remote/` is the hosted remote: a Cloudflare Worker with Durable Objects
  over Artifacts repos, deployed at `https://newgit.dev`.
- `.newgit/` is created by `newgit init`. Committed: `config.toml`,
  `trackers/`, `resources/`, `scripts/`. Gitignored local state: `branches/`,
  `checkpoints/`, `logs/`, `state/`, `installs/`, and `local/` — which holds
  `trackers.git`, every tracker's history (bare repo).

