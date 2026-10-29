- `newgit claude-code install` gives Claude Code's worktrees newgit
  instances: `claude --worktree <name>`, a subagent with `isolation:
  "worktree"`, and a background session each spawn an instance named for
  the worktree, with its trackers projected and its resources prepared,
  and removing the worktree runs `newgit remove`.

  It writes Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks into
  `.claude/settings.local.json` (`--shared` writes `.claude/settings.json`
  instead), and refuses beside another `WorktreeCreate` hook rather than
  have two race to make the worktree. The hooks are `newgit claude-code
  worktree-create` and `worktree-remove`, which read Claude Code's hook
  input on stdin and print nothing on stdout but the workspace path.
