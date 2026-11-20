# Using it with T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) gives each agent thread its
own Git worktree. A fork,
[`Spheroman/t3code`](https://github.com/Spheroman/t3code), gives it a
newgit instance instead. In a project with `.newgit/config.toml` at its
root, "New worktree" runs `newgit spawn <branch> --from <base> --json` and
opens the thread in that workspace, with its trackers bound and its
resources prepared. Deleting the thread runs `newgit remove`, which
checkpoints the workspace before deleting it, so a thread deleted with
work in it can be brought back (`newgit spawn <name> --from-checkpoint
<name>/<id>`; the id is `checkpoint` in `remove --json` and `removed_to` in
the archived record). A thread whose workspace was evicted is not
checkpointed again: the eviction's checkpoint already holds it, and is the
one named. A resource whose capture fails, such as a dump against a
database that is down, does not stop the delete when its state is
outside the workspace (a Supabase stack, a shared server's database):
its `[cleanup]` does not run, so the delete never drops a database
nothing kept, and `remove` names that state under `left`, with the
cleanup command that removes it by hand, and what else it could not keep
under `lost`. When its state is in the workspace (`ownership =
"workspace"`, as the `postgres` template's cluster is), deleting the
workspace would delete it, so `remove` refuses and deletes nothing
(`remove --json` then prints `refused`, with a `reason` and, when starting
the instance would let the capture run, `next`). The
fork removes a newgit workspace before its thread, so the thread is not
deleted either: it starts the instance (`newgit start <name>`) so the dump
can run, and removes again. If newgit still refuses, the fork stops the
instance again and shows newgit's reason, and you choose to delete anyway
(`newgit remove <name> --force`) or keep the thread and its workspace. Every
other project gets plain worktrees, as before.

`newgit stop <instance>` and `newgit start <instance>` are the pair for
settling and unsettling a thread
([#157](https://github.com/Spheroman/newgit/issues/157)): `stop` stops every
running long-running action, dependents first, and records what it stopped
in `.newgit/state/<slug>/stopped.toml`; `start` brings back exactly
that, dependencies first, so a long-running action without `autostart`
that nobody started stays stopped, plus any `autostart` action that died on its own or that a spawn
withheld while a dependency was not answering (a dead one `status` shows as
`exited(1)`, `killed(SIGKILL)` or `crashed`, not `stopped`).
Port claims are kept, so the instance returns on the same ports. With
`--json` each prints `{"instance", "resources": [{"name", "action",
"status", "error"}]}`, listing only what it acted on, and still exits 1 if
anything failed. `start` then waits, as `spawn` does, for what it brought
back to pass its `[ready]` check, adds `starting` and `problems` to its
JSON when there are any, and exits 1 when the instance is not ready; a resource that failed to start stays recorded for the
next `start`. `spawn` exits 1 the same way. The fork reads either as
started but not ready, not as a failure: the thread opens in the workspace,
its turn runs, and newgit's `problems` are shown once in the thread. A
workspace deleted by hand comes back on the thread's next message, since
`start` rebuilds it. A resource that is not a process newgit supervises — a
Supabase stack, a Compose project — is taken down only if it defines a
`suspend` / `resume` pair (`newgit reference actions`); the `supabase`
template does. The fork runs `stop` when you settle a thread and `start`
when you unsettle it or send it a message
([Spheroman/t3code#2](https://github.com/Spheroman/t3code/issues/2));
`remove` stays for delete.

To give a settled thread's disk back, `newgit stop --evict <instance>`
checkpoints it and deletes its workspace, all but its marker, and the
`start` on unsettle brings it back from that checkpoint at the same path.
The fork evicts once the settle finishes and no terminal runs there, unless
**Free disk when a newgit thread settles** is off in Settings → Threads.
An eviction that would lose something (ignored build output nobody
declared, a rebase in the middle, a shell still in the directory) refuses
and leaves the thread stopped, on disk. The server has to run it with its
working directory outside the workspace: run from inside, it refuses, since
the directory would be deleted under the caller. A Rust workspace is
freed once its build output is declared disposable, in a resource such as
`.newgit/resources/build-output.toml` holding `ownership = "workspace"`
and `disposable = ["target"]` (#447): the eviction drops `target/` and
says so, and the build refills it after `start`. Each settle/unsettle
cycle leaves an evict checkpoint behind, often the only checkpoint of that
turn's end state. `newgit cleanup` collects one only after an unsettle
restored it and a later settle superseded it (#446), never the one a
deleted thread was evicted to. It deletes the checkpoint's refs; the
store's own `git gc` frees the disk later.

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
  endpoint, which `gh` cannot read, so give root's definition an `export`
  with `branches = true` (`newgit reference export`): each push then lands
  the branch there too, and the workspace's remote for it is the one `gh`
  uses.

The fork is a small patch stack that merges a fast-moving upstream in; its
[`FORK.md`](https://github.com/Spheroman/t3code/blob/newgit/FORK.md) lists
every change, its limits, and how to bring upstream in.
