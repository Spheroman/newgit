# Behavior worth knowing

**`export` fails closed.** Source ships (its audience is everyone) and so do
trackers whose audience is `public`. Anything narrower is withheld and
reported; `--include <path>` overrides. It writes one commit, not history,
because exporting the branch's commits would carry any file they contain —
including withheld ones. This is a path-level filter, not concealment.

**GitHub stays where it is.** `newgit export --remote github` pushes a
composed history to an ordinary repository defined in
`.newgit/exports/github.toml` (`url = "acme/widget"`, `trackers =
["root"]`). A target ships exactly the trackers it names, or the `public`
ones if it names none, so a
public mirror and a private one can show different views, and a history that
ever committed the file of a tracker outside Git is refused. Exporting again
fast-forwards, after bringing in whatever was merged on the target since,
and the push uses your own Git credentials, so `gh`, GitHub
Actions, and anyone cloning from GitHub keep working. With `branches =
true`, every push from a workspace exports its branch there too, and on a
newgit remote the workspace gets the target as the remote `gh` resolves, so
an agent's `git push` then `gh pr create` opens a real pull request.

**Tags are the project's.** On a newgit remote, `git tag v1.0 && git push
origin main v1.0` (or `git push --follow-tags`) in the project root names
that point of `main` on the remote, then on each export target, each at its
own view of the commit; a GitHub release made on a mirror comes back as the
same tag. `git fetch` brings tags from the remote like any other. A tag
never moves, and names only what `main` has: tag from the project root once
the work is merged, not from a workspace.

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
your port does not. A render writes only a regular file of its own: a
rendered path that is a symlink, or lies under a symlinked directory, is
refused by name rather than written through to wherever the link leads,
and a hard link there is replaced by a new file, so its other names keep
what they held ([spec](../design/spec.md#render)).

Several resources may render one file — `db` its URL and `web` its port into
the app's `.env.local` — each declaring only the values it owns. They apply
as one batch, so the order they bind in does not matter; two resources
claiming the same text are refused, naming both.

A file rendered once is not read again by a process already running on it.
When `tracker use` or `drop` changes a rendered file's bytes (binding a
per-branch database writes its port into `apps/admin/.env.local`, say), each
process that was running before restarts if its resource declares `reads`
covering the file, and is named with the command that restarts it if it
declares nothing:

```toml
reads = ["apps/admin"]   # a dev server run from apps/admin reads its .env.local
```

A render can also target a file inside a resource's own `[identity]
produces`, a built tree rather than source. That is how an install whose
tree records its workspace path can still be shared through the install
store (`newgit reference render`).

**A workspace's `origin` is the real remote.** `git pull` and `gh` see the
project's repository, and `git push` checkpoints the instance before
forwarding the push there — succeeding only if that remote accepted it.
`gh pr create` works from the workspace with no flags.

**Branching inside a workspace is fine.** `git checkout -b try-x` in an
instance's workspace works the way Git does. A checkpoint captures `try-x`
as it stands, and undo returns to it, but the instance's own branch stays
where it was until you switch back and commit there. `status` shows
`` feature-a@1a2b3c4d (workspace on `try-x`) `` while that is the case.

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
