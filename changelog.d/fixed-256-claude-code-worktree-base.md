- Claude Code worktrees start from the project branch as it stands, not
  the project root's checked-out `HEAD`
  ([#256](https://github.com/Spheroman/newgit/issues/256)).

  The `WorktreeCreate` hook ran a plain `newgit spawn`, so every subagent
  started from the root's local `main`. When nobody had pulled, that was
  stale: in one run it was 20 commits behind `origin/main`, and each agent
  rebased itself by hand. The hook now does what Claude Code's own
  worktrees do. By default it uses the project branch, fetching its
  upstream if the root has not fetched in a day, and takes whichever of
  the two is newer. A root branch that is ahead or has diverged wins. The
  choice is made under a store-wide lock, so subagents launched together
  start from the same commit. A failed fetch is not retried by the rest
  of the burst. `worktree.baseRef = "head"` keeps the old behaviour.

  The first spawns in a project, run at once, no longer fail on each
  other while creating the publish outbox.
