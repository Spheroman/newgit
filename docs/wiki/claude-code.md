# Using it with Claude Code

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

The instance starts where Claude Code's own worktree would, not at
whatever the project root has checked out (which is what a plain `newgit
spawn` uses):

- By default (`worktree.baseRef` unset or `"fresh"`), the project branch
  as it stands. If nothing in the project root has fetched for a day, the
  hook first fetches that branch's upstream (5 seconds at most, never
  prompting; on failure it uses the ref already there). Then it takes
  whichever of the root's branch and its upstream contains the other: the
  upstream when the root's `main` is only behind (a PR merged on GitHub
  that nobody pulled), the root's branch when it is ahead or the two have
  diverged. The root's own branch is never moved.
- Subagents launched together make their hooks run at once. The choice is
  made under one store-wide lock, so one of them fetches and they all
  start from the same commit. If that fetch fails, the hooks in the next
  minute don't retry it. They use the ref already there, as the one that
  failed did.
- With `worktree.baseRef = "head"`, it starts from the root's `HEAD`. For a
  session already in an instance, it starts from that instance's source
  branch as the store last saw it (its last checkpoint or push).
- A name that is already a Git branch in the project spawns on that
  branch as it is.
- The hook prints the base it chose on stderr, which ends up in Claude
  Code's debug log.

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
