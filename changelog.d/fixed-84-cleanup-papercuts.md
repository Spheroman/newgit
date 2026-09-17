- Two papercuts from one integration session
  ([#84](https://github.com/Spheroman/newgit/issues/84)).

  `cleanup` said `Nothing to clean up.` and then, immediately below,
  described eleven snapshot revs it had kept and why. The retention report
  is the useful part — it is what sends you to `--purge-archived` — but the
  line above it made the whole output read as self-contradictory, and the
  first reaction is to reread it working out which half is wrong. It now
  says `Nothing to remove.`, which is the true and narrower claim and
  leaves the retention report free to explain what was kept.

  `spawn` now says when it is continuing checkpoint numbering left behind
  by a previous instance of the same name. Numbering is per name and
  `remove` archives the binding record without deleting the checkpoint
  files, so a name that has been removed and spawned again takes its first
  checkpoint as `ckpt_004` on an instance thirty seconds old — the one
  thing that carried over when the workspace, containers and volumes were
  all destroyed and rebuilt, which is the opposite of what the rest of
  `remove` implies.

  Restarting at `ckpt_001` was the other option and is not available: the
  old `ckpt_001.toml` is still on disk — releasing it is exactly what
  `newgit cleanup --purge-archived` is for — and reusing the id would make
  it ambiguous which of two checkpoints `newgit undo ckpt_001` meant. So
  `spawn` states it at the moment it becomes true instead, on a `history:`
  line naming the count, the id the first checkpoint here will get, and the
  command that releases them.
