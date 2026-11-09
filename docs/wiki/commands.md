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
  `main`s, history and all, to the target in `.newgit/exports/<name>.toml`
  (a GitHub mirror, say); exactly the trackers it names, or the `public`
  ones if it names none
- `newgit cleanup [--dry-run] [--purge-archived]`
- `newgit ports [--check]` — claimed ports, or detect a listening one nothing
  claims
- `newgit render [--check]` — resolve every `[[render]]`; `--check` tests
  each `find` against the working tree without spawning, for adoption and CI

Definition management (noun subcommands):

- `newgit tracker create <name> [--audience <a>] [--storage <s>]
  [--merge-with-source]`
- `newgit tracker create <name> --git --path <path>` — a tracker in Git,
  mounted at that path by one commit; history before it stays in whichever
  tracker held the files
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
- `newgit remote add [--url <url>] [--project <name> | <owner>/<name>]
  [--json]` / `remote show [--json]` — a bare name creates the project under
  your account; `owner/name` joins one that exists
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
- `newgit members add|remove <github-login>` / `members list [--json]`
- `newgit spawn <name> --from-checkpoint <instance>/<checkpoint>`

Resource templates: `process`, `pnpm`, `install`, `command-snapshot`,
`command-snapshot-migrations`, `supabase`, `external`. `newgit resource
templates --show <name>` prints one in full without creating anything. Their
comments say what to edit: `process` works without a port (delete `[ports]`,
`[exports]` and `[[render]]`), and `supabase` needs every service's section
in `config.toml` and has a commented `workdir` for monorepos.
