- `spawn --from-checkpoint` leaves the new instance's workspace on its own
  branch again
  ([#315](https://github.com/Spheroman/newgit/issues/315)).

  Since 0.8.0 a checkpoint records the branch its workspace had checked
  out, and an undo puts the workspace back on it. The undo that finishes
  the spawn put the new workspace on the author's branch instead: after
  `spawn review --from-checkpoint feature-a/ckpt_001`, `review`'s
  workspace was on `feature-a`, work committed there never reached
  `review`, and each checkpoint there warned that `review` stayed put.
  The imported checkpoint now reads as taken on the new instance's
  branch, so the spawn starts there, as it did before 0.8.0. That holds
  too when the author's workspace was on another branch, such as
  `try-x`, or detached: `spawn` makes the new branch at the checkpoint's
  commit either way.
