# Commands

Branch-instance lifecycle (top-level verbs; `[instance]` is inferred when run
inside a workspace):

- `newgit init [--name <name>]` / `spawn <name> [--from <rev>] [--use
  <tracker>] [--drop <tracker>] [--private] [--json]` / `status [name]` /
  `remove <name> [--purge] [--keep-branch | --delete-branch]` — `status`
  says whether each instance is ready, still starting (a dev server that is
  not listening yet), or not ready, and why
- `newgit status <instance> --path` — just the workspace path, for scripts
- `newgit reference [section | all]` — the definition format, every key and
  default; bare, it lists the sections
- `newgit run [instance] -- <command>` — run with exports and ports loaded
- `newgit env [instance]` — the environment `run` and every hook get, each
  variable labeled with the declaration it came from
- `newgit action <resource>.<action> [instance] [--dry-run]` — `--dry-run`
  prints the rendered command and its directory, and runs nothing
- `newgit stop [instance] [--json]` / `start [instance] [--json]` — stop
  every running long-running action and run each `suspend`, dependents
  first, and later bring back exactly what that took down (and any
  `autostart` action that died on its own); ports,
  workspace, trackers, and installs stay as they are. Both exit 0 with
  nothing to do and 1 if any resource failed
- `newgit checkpoint [instance] [-m <msg>] [--verify]` / `undo [instance]
  [--to <id>] [--only <resource>] [--force-recompute]` / `checkpoints
  [instance]` — `--verify` proves the restore works by running a real undo;
  `--only` restores one resource and leaves everything else alone
- `newgit export [instance] --to <dir> [--include <path>] [--exclude <path>]`
- `newgit export --remote <name> [--dry-run] [--force]` — push the trackers'
  `main`s, history and all, to target `<name>` (a GitHub mirror, say), which
  a tracker's `export` declares: that tracker and every tracker inheriting
  its export ship, nothing else. A target is named after its repository
  unless its `export` gives a `name`
- `newgit cleanup [--dry-run] [--purge-archived]`
- `newgit ports [--check]` — claimed ports, or detect a listening one nothing
  claims
- `newgit render [--check]` — resolve every `[[render]]`; `--check` tests
  each `find` against the working tree without spawning, for adoption and CI

Definition management (noun subcommands). Wherever a command takes a
tracker, it takes a path, read from where it runs: `t3code/repos` at the top
of the project, `repos` inside `apps/t3code`, `..` to go up:

- `newgit tracker create <name> [--audience <a>] [--storage <s>]
  [--merge-with-source]`
- `newgit tracker create <name> --git --path <path>` — a tracker in Git,
  mounted at that path by one commit; history before it stays in whichever
  tracker held the files. Inside another tracker's mount it is that
  tracker's child
- `newgit tracker create <name> --git --mount <dir> --from <repo>
  [--from-branch <branch>]` — bring in another repository's history
  unchanged, mounted at an empty directory
- `newgit tracker link <name> <owner>/<project>[/<path>] --mount <dir>` —
  mount a tracker in Git that lives in another project on the remote; its
  history and policy stay its home's
- `newgit tracker track <tracker> <path>...` — trackers outside Git only
- `newgit tracker remove <name>`
- `newgit tracker capture|merge|pull|checkout <tracker> [instance]`
- `newgit tracker capture <tracker> --from-store` — seed a lane from the store
  repo's working tree, so the first `spawn` comes up with the content
- Capture never follows a symlink: a link that stays inside the workspace is
  kept as a link, as Git keeps one; a link out of the workspace, or a tracked
  path under a symlinked directory, is refused by name
  ([spec](../design/spec.md#trackers-outside-git))
- `newgit tracker list` — a tree by ownership: whether it is in Git, role, priority,
  maintainers, and (inside a workspace) what is bound. Trackers with no
  `paths` come after the tree, with what deposits into them and what they
  bring. `--json` lists every tracker with `default_bound` (what a `spawn`
  with no `--use`/`--drop` binds) and `bound` (what this instance binds,
  `null` outside one), named as `--use` takes them
- `newgit tracker use <tracker> [instance]` / `newgit tracker drop <tracker>
  [instance] [--stash | --discard]` — a drop parks the resources the tracker
  brought: taken down by `suspend` (else `[cleanup]`), ports kept, and
  brought back on them by the next `use`; `remove` tears parked ones down
- `newgit tracker migrate [--dry-run]` — move a project laid out before
  mounts (a `project` tracker owning `.newgit`) onto roots and mounts.
  Recomposes every branch and checkpoint first and moves nothing unless the
  ids come out the same. A force push of tracker history, so run it once,
  after upgrading
- `newgit log <tracker> [instance] [-- <git log args>]` — one tracker's
  history: an instance's branch as of its last checkpoint, or the project
  root's
- `newgit resource add <name> --template <template>` / `list` / `templates`
- `newgit resource remove <name> [--force]` — `--force` drops it from live
  instances and releases their ports instead of refusing
- `newgit resource rebind <name> [instance]` — bind it again as spawn did,
  after fixing a render or export that failed

Remote:

- `newgit login` / `logout` / `whoami`
- `newgit serve --data <dir> [--listen <addr>] [--url <url>] [--allow <logins>]
  [--github-client-id <id>]` — run a remote on this machine ([your own
  remote](serve.md)); `newgit serve --help` lists the rest
- `newgit remote add [--url <url>] [--project <name> | --project
  <owner>/<name>] [--json]` / `remote show [--json]` — a bare name creates
  the project under your account; `owner/name` joins one that exists.
  `remote show` also says what the remote has not caught up on yet
- `newgit remote delete <owner>/<name>` — the owner deletes the project
  from the remote: every repo, record, and token pinned to it. Not undoable
- `newgit clone <owner>/<name> [dir] [--url <url>] [--full]` — root's whole
  history and the commits it pins; a mounted tracker's history comes the first
  time something needs it, once. `--full` fetches every history now
- `newgit merge [instance] [--into <branch>]` — the merge commit is made in
  your view and committed at root's ref; `newgit merge main` merges the
  project root
- `newgit pull <tracker> [instance]` — one commit moving a mounted tracker's
  pin to its `follow` ref; spawn says when a tracker is behind
- `newgit pull` — every tracker in Git root mounts, in one commit on the
  branch it runs on (`Update trackers: t3code (14 commits), ui-kit (2
  commits)`; in the project root, its own instance branch, which `newgit
  merge main` publishes); no commit when none is behind. A pin holding
  commits its ref lacks stays put, a tracker you can't read on the remote is
  skipped (it fails when none can be read), and trackers outside Git are
  left alone (`newgit tracker pull`)
- `newgit pull --remote <target>` — bring back what was merged on an export
  target (a pull request on GitHub); `newgit merge main` publishes it
- `newgit members add|remove <github-login>` / `members list [--json]`
- `newgit spawn <name> --from-checkpoint <instance>/<checkpoint>` — one
  taken on this machine is used as it is; any other comes from the remote

Resource templates: `process`, `pnpm`, `install`, `command-snapshot`,
`command-snapshot-migrations`, `supabase`, `external`. `newgit resource
templates --show <name>` prints one in full without creating anything. Their
comments say what to edit: `process` works without a port (delete `[ports]`,
`[exports]` and `[[render]]`), and `supabase` needs every service's section
in `config.toml` and has a commented `workdir` for monorepos.
