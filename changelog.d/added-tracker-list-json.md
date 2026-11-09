- `newgit tracker list --json` lists every tracker with what a new
  instance binds by default (`default_bound`) and what this instance binds
  (`bound`), so a tool can offer the choice `spawn --use`/`--drop` make.
  T3 Code's new-worktree panel uses it: a checkbox per tracker and a
  dropdown per role, and its agents choose the same way through MCP.
