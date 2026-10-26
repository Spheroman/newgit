# T3 Code, newgit fork

This fork of [pingdotgg/t3code](https://github.com/pingdotgg/t3code) adds
[newgit](https://github.com/Spheroman/newgit) support. In a project with
`.newgit/config.toml` at its root, "New worktree" creates a newgit instance
instead of a Git worktree: `newgit spawn <branch> --from <base> --json`
makes the branch, its workspace, and its bound trackers and resources, and
deleting the thread runs `newgit remove`. Every other project gets plain
worktrees, as upstream does.

## Branches

- `main` mirrors upstream.
- `newgit` (default) is upstream plus the patch stack, one commit per
  concern:
  1. newgit workspaces in place of worktrees (server only)
  2. a desktop identity that installs beside upstream T3 Code
  3. fixes to that identity

## Install (macOS)

```sh
git clone https://github.com/Spheroman/t3code && cd t3code
scripts/fork/install-mac.sh
```

It builds an unsigned app for this Mac's architecture and installs
`/Applications/T3 Code (newgit).app`. Nothing it owns collides with
upstream T3 Code, so both run at once:

|                   | Upstream              | This fork                                  |
| ----------------- | --------------------- | ------------------------------------------ |
| App ID            | `com.t3tools.t3code`  | `com.spheroman.t3code-newgit`              |
| State             | `~/.t3`               | `~/.t3-newgit`                             |
| Electron profile  | `t3code-v2`           | `t3code-newgit`                            |
| Keychain item     | `t3code Safe Storage` | `t3code-newgit Safe Storage`               |
| CLI command       | `t3`                  | `t3code-newgit`                            |
| `t3code://` links | handled               | not registered                             |
| Auto-update       | on                    | off, so upstream releases never replace it |

## Settings

- `T3CODE_NEWGIT=0` turns newgit workspaces off.
- `T3CODE_NEWGIT_BIN` names a `newgit` that is not on the server's `PATH`.
  If `newgit` cannot run, the thread gets a Git worktree and the server
  logs why.

## Limits

- Threads keep their temporary `t3code/<hex>` branch name: upstream renames
  a worktree's branch after the first message, and a newgit instance cannot
  be renamed.
- Storage cleanup skips newgit workspaces; `newgit cleanup` owns them.
- A missing newgit workspace is not recreated on resume, and MCP
  `existing_worktree` launches reject newgit workspaces, since they are
  separate clones rather than linked worktrees.
- PR review guides still use their own temporary Git worktrees.
- Sign-in flows that return through a `t3code://` link reach upstream
  T3 Code, not this app.
- Eleven upstream desktop and build-script tests assert upstream's app ID,
  state directory, and CLI name, so they fail here. They are left as they
  are to keep rebases clean.

## Keeping up with upstream

The newgit logic lives in `apps/server/src/vcs/newgitWorkspaces.ts` and
`apps/desktop/src/forkIdentity.ts`. Every edit to an upstream file is marked
`newgit fork patch`, so `git grep "newgit fork patch"` lists every hook.

```sh
git remote add upstream https://github.com/pingdotgg/t3code  # once
git fetch upstream
git switch main && git merge --ff-only upstream/main && git push origin main
git switch newgit && git rebase main
git grep -n "newgit fork patch"   # check each hook still sits where it should
git push --force-with-lease origin newgit
scripts/fork/install-mac.sh
```
