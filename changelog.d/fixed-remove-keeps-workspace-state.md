- `newgit remove` no longer deletes a database whose dump failed when the
  database lives in the workspace.

  A resource whose capture fails does not stop a removal, and its
  `[cleanup]` does not run, so state outside the workspace (a Supabase
  stack, a shared server's database) is left where it is and named under
  `left`. That cannot work for state inside the workspace: deleting the
  workspace deleted it, and `remove` exited 0 while saying the database was
  "left in place". Now a failed capture of a resource with `ownership =
  "workspace"` (the `postgres` template's cluster) refuses the removal,
  deleting nothing, unless `--force`, which names the state as lost.
  Ownership is the signal because newgit cannot see where a command keeps
  what it captures; the reference's ownership table now says `workspace`
  means kept in the tree. The `left` line says the state is outside the
  workspace because its `ownership` says so, and the ignored files a
  removal deletes are no longer described as "rebuilt by no resource" when
  a resource's own capture (a dump) is in the checkpoint. `newgit
  checkpoints <name>` now lists a removed instance's checkpoints, which
  `remove` points at, instead of saying there is no such instance.
