- `tracker list` shows trackers with no `paths`
  ([#247](https://github.com/Spheroman/newgit/issues/247)).

  The list is a tree by ownership, and a tracker that owns no paths had no
  place in it, so it was silently left out — although the list is where you
  check what `--use` accepts, and the `supabase` template creates one. Such
  trackers now come after the tree with their role, and PATHS says what they
  are for: `(no paths; deposits from db; brings db, functions)`. A spawn
  from a checkpoint also says `deposit-only` for them rather than
  `(0 files)`, matching `spawn` and `tracker pull`.
