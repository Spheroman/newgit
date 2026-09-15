- Resources can set `workdir`, overridable per action, so a monorepo
  definition does not have to start every command with the same `cd`.

  A resource whose real work lives at `packages/db/supabase` used to repeat
  `cd packages/db/supabase && ...` on `prepare`, `stop`, `checkpoint`,
  `restore`, and `cleanup` alike — seven lines, one prefix, and a `&&` that
  silently swallows a failed `cd` because the command after it still runs.
  `workdir = "packages/db/supabase"` at the top of the resource says it once;
  newgit spawns the command there directly (`Command::current_dir`, not a
  shell prefix), so there is no `&&` left to bind wrong. An action can set
  its own `workdir` to replace it for just that one command.

  It only ever changes where a *command* runs. `[identity].paths`,
  `[checkpoint].paths`, and `[[render]].path` stay workspace-root-relative
  regardless — those are content paths, and a definition that had to track
  two roots at once would be worse than the `cd` it replaces.
