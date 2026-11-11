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
- `newgit stop --evict [--discard-ignored]` — stop, checkpoint, and delete
  the workspace, leaving its marker; `newgit start` brings it back from that
  checkpoint into the same directory, with uncommitted and staged work,
  other branches, the stash, notes, other remotes' refs, local Git config,
  hooks, file modes, empty directories, tracker content, and rebuilt
  installs. It refuses, deleting nothing, when something would not come
  back: ignored files no tracker owns and nothing rebuilds (listed, a
  directory with its file count; `--discard-ignored` lets them go, and
  declaring disposable build output such as `target/` is not possible yet,
  #447), a
  merge or rebase in the middle, hand edits to a rendered file, an untracked
  repository, an initialized submodule, a linked worktree, or a process
  still in the directory (run it from outside the workspace). Not kept, and
  said first: reflogs other than the stash's, intent-to-add entries,
  hand-set assume-unchanged/skip-worktree flags, setuid/setgid/sticky bits,
  and hand-set values on config keys newgit sets itself. Evicting does not
  free an untracked file's blob, and evict checkpoints are not pruned yet
  (#446). A restore that fails or is cut short moves what is at the path
  aside, whole, to `.<name>.restore-failed` or `.<name>.interrupted-restore`
  and keeps it; `start`, `status`, `stop --evict`, and `cleanup` name each
  one until `newgit cleanup --discard-saved <path>` deletes it
- `newgit remove <instance> [--force]` — on an evicted instance whose path
  holds work written after a failed restore, refuses unless `--force`
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
  [--role <r>] [--merge-with-source]`
- `newgit tracker create <name> --from <tracker> [--role <r>] [--audience
  <a>]` — a fork of a tracker outside Git: its history starts at that
  tracker's `main` (nothing is copied), it claims the same paths and fills the
  same role, and its audience may be narrower than its upstream's, never wider
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
- `newgit pull [<tracker>...] [--instance <name>]` — every tracker in Git
  mounted here, at any depth, or only those named, in one commit on the
  branch it runs on (`Update trackers: t3code (14 commits), ui-kit (2
  commits)`; in the project root, its own instance branch, which `newgit
  merge main` publishes); no commit when none is behind. A tracker exported
  to a target rooted at it first takes what is new there, as `--remote`
  would (one more fetch each), without moving its `main` until `newgit
  merge`. A child moves by a commit in the tracker that mounts it, whose pin
  then moves, up to root. A pin holding commits its ref lacks stays put: an
  instance with commits of its own on a tracker is listed as diverged and
  does not take its target either, until `newgit merge` lands that work. A
  tracker you can't read on the remote, or whose export target can't be
  reached, conflicts, or adds a file of a tracker outside Git, is skipped
  and named while the rest move (it fails when none can be read); a child
  of a tracker the instance doesn't bind stays, and is named when it was
  asked for or is behind; trackers outside Git are left alone (`newgit
  tracker pull`). A named tracker that stays exits non-zero. Every argument
  is a tracker: `--instance` names another instance. Spawn says when a
  tracker is behind. Two instances pulling the same two diverged tips make
  the same merge commit (newgit's, dated from its parents), so merging both
  lands one merge. It is unsigned, so a target that requires signed commits
  rejects an export carrying it
  ([#443](https://github.com/Spheroman/newgit/issues/443))
- `newgit pull --remote <target>` — bring back what was merged on an export
  target (a pull request on GitHub); `newgit merge main` publishes it. A
  force-pushed target is merged with the line the tracker already has, as
  `git pull` would: the commits the force-push removed stay in the
  tracker's history, and the next export pushes them to the target again.
  Reverting the change in an instance keeps it out of the tip only: the
  next export still publishes those commits, so a secret the force-push
  purged is published again. Nothing in newgit can purge it yet
  ([#442](https://github.com/Spheroman/newgit/issues/442)); until then,
  do not export that tracker
- `newgit tracker allow-plaintext <path>...` — release workspace paths this
  machine has held encrypted content at, so whatever tracker claims them now
  (a `public` one, or one in Git) may capture, commit, and send them
  unencrypted. Until then every such capture and send fails: definitions
  arrive from elsewhere. A directory releases every held file under it, and
  case does not matter. The guard knows paths, not content: the same
  content copied or moved to another path is not held back. A
  confirmation, not a boundary: anything that can run commands here can
  run it
- `newgit tracker pull <tracker> --keep-mine` — after a `tracker merge` was
  refused because the remote's `main` moved: pull it and put this
  instance's version back on top as a new capture (each file you have
  replaces theirs, a file you deleted since the rev you started from is
  deleted again, files only they added stay; with no shared rev, a file you
  deleted comes back, and it says so), so merging again publishes it
- `newgit members add|remove <github-login>` / `members list [--json]` — add
  and remove re-encrypt each shared encrypted tracker's `main` for the new
  members, list every recipient by key fingerprint (saying `CHANGED` when
  the set is not the one this machine last used), and warn about a member
  with no usable SSH key on GitHub; they fail, naming the trackers that were
  not re-encrypted, when any was not. `members reencrypt` runs that again
- `newgit spawn <name> --from-checkpoint <instance>/<checkpoint>` — one
  taken on this machine is used as it is; any other comes from the remote

Resource templates: `process`, `pnpm`, `install`, `command-snapshot`,
`command-snapshot-migrations`, `supabase`, `external`. `newgit resource
templates --show <name>` prints one in full without creating anything. Their
comments say what to edit: `process` works without a port (delete `[ports]`,
`[exports]` and `[[render]]`), and `supabase` needs every service's section
in `config.toml` and has a commented `workdir` for monorepos.
