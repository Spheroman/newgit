- `undo` no longer prints a bare `ok` for a resource it leaves failed,
  blocked or pending ([#211](https://github.com/Spheroman/newgit/issues/211)).

  Since #137 a restore that runs nothing (`none`, `external`, no
  `[restore]`) leaves a resource's status as it found it, so a `prepare`
  that failed before the undo is still failed after it. The report's `ok`
  is the verdict on the restore action, which could not fail; under
  "Restored" it read as the resource being fine while `status` showed it
  failed. The line now adds what the resource is left as, the way `status`
  derives it — `none ok, left failed`, `none ok, left blocked by deps` — and
  `UndoResourceOutcome` carries it as `status` and `blocked_by`. A restore
  that was ok but whose restart failed now says `restart FAILED` and counts
  against "N of M resources restored" and the "may be in a partial state"
  list; before, that list could name nobody.
