- `undo` no longer prints a bare `ok` for a resource it leaves failed or
  pending ([#211](https://github.com/Spheroman/newgit/issues/211)).

  Since #137 a restore that runs nothing (`none`, `external`, no
  `[restore]`) leaves a resource's status as it found it, so a `prepare`
  that failed before the undo is still failed after it. The report's `ok`
  is the verdict on the restore action, which could not fail; under
  "Restored" it read as the resource being fine while `status` showed it
  failed. The line now adds the status the resource is left in — `none ok,
  left failed` — and `UndoResourceOutcome` carries it as `status`.
