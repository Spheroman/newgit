# Adopting it in a project

```sh
cd your-project        # any Git repository with at least one commit
newgit init
```

`init` prints what to commit. Run it again where `.newgit/` already exists
(a plain `git clone` of a newgit project, or one set up by an older newgit)
and it adopts what is there: it writes any missing built-in tracker
definitions and gives `root` the project's history. The split matters:
`.newgit/config.toml`, `trackers/`, `resources/`, `scripts/`, and `exports/`
are the control plane and belong in Git, so teammates and CI see the same orchestration. Everything else under
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

## What installs cost

Every branch instance gets its own dependency tree. Two branches with
different lockfiles must not share one, or one branch's install silently
rewrites the other's, which is why installs are resources rather than
trackers.

Two instances at the *same* lockfile are one tree built twice, though. An
install that declares what it produces (`[identity] produces`) is built once
and published to the install store in `.newgit/installs/`; every later
instance with the same key is filled from it with a copy-on-write clone and
never runs the install. The key covers the lockfile, the definition file,
the `key_command` output, and which paths the instance checks out (plus
which trackers it binds, when the install is told them through
`{{bound…}}`). Where the filesystem cannot clone (APFS, btrfs, and XFS
can), newgit takes a full copy and says so. A tree that names its own
workspace in a file is not shared, unless a `[[render]]` on that file lets
the store rewrite it per instance; the `pnpm` template does this for pnpm's
workspace state.

On a miss, the cost is your package manager's. **pnpm**, uv, and bun
hardlink from a shared cache, so a new tree is mostly directory entries.
**npm** and `pip` into a venv expand a full copy each time. `newgit reference
installs` covers the store in full: what goes into the key, what `cleanup`
prunes, and how `undo --force-recompute` drops an entry it no longer trusts.
