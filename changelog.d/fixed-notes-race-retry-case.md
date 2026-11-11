- Two merges writing notes into the same store at once no longer fail on
  Git versions that capitalise the warning.

  When `git fast-import` loses the race for the notes ref, newgit builds
  its notes again on the other writer's and retries. It recognised the
  lost race by matching `not updating` in Git's output, but some Git
  versions (2.45.2, for one) print `warning: Not updating refs/notes/...`
  with a capital N, so the retry never ran and the second merge failed
  instead. The match is now case-insensitive.
