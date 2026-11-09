# Using it with T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) gives each agent thread its
own Git worktree. A fork,
[`Spheroman/t3code`](https://github.com/Spheroman/t3code), gives it a
newgit instance instead. In a project with `.newgit/config.toml` at its
root, "New worktree" runs `newgit spawn <branch> --from <base> --json` and
opens the thread in that workspace, with its trackers bound and its
resources prepared. Deleting the thread runs `newgit remove`. Every other
project gets plain worktrees, as before.

`newgit stop <instance>` and `newgit start <instance>` are the pair for
settling and unsettling a thread
([#157](https://github.com/Spheroman/newgit/issues/157)): `stop` stops every
running long-running action, dependents first, and records what it stopped
in `.newgit/state/<slug>/stopped.toml`; `start` brings back exactly
that, dependencies first, so a resource nobody had started stays stopped,
plus any `autostart` action that died on its own (`status` shows it as
`exited(1)`, `killed(SIGKILL)` or `crashed`, not `stopped`).
Port claims are kept, so the instance returns on the same ports. With
`--json` each prints `{"instance", "resources": [{"name", "action",
"status", "error"}]}`, listing only what it acted on, and still exits 1 if
anything failed; a resource that failed to start stays recorded for the
next `start`. A resource that is not a process newgit supervises — a
Supabase stack, a Compose project — is taken down only if it defines a
`suspend` / `resume` pair (`newgit reference actions`); the `supabase`
template does. The fork runs `stop` when you settle a thread and `start`
when you unsettle it or send it a message
([Spheroman/t3code#2](https://github.com/Spheroman/t3code/issues/2));
`remove` stays for delete.

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
