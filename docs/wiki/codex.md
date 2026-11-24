# Using it with Codex

Codex makes its own Git worktrees (`codex --worktree`, and each worktree
thread in the desktop app), and has no hook that lets another tool make the
worktree in its place, as Claude Code's `WorktreeCreate` does. So there is
no `newgit codex install`. A newgit workspace is a full clone with its
trackers bound and its resources prepared, and a worktree Codex made is
neither.

Spawn the instance yourself and start Codex in it:

```sh
newgit spawn auth-refactor --json      # prints the workspace path
codex -C <workspace>                   # a session in that instance
```

In the desktop app, open the workspace as a project and use it in *Local*
mode, not *Worktree*: a worktree thread would be a Git worktree of the
workspace, without the instance's trackers or resources.

- Leave `--worktree` off. Codex then works in the directory it was given.
- Nothing removes the instance when the session ends: `newgit remove
  auth-refactor` when you are done, which checkpoints it first.
- A session pushes and opens pull requests the usual way, `git push` then
  `gh pr create`, as in any workspace.

This was written from Codex's CLI (0.167) and desktop app as installed, and
its documentation, not from a signed-in session.
