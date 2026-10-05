- Composing keeps each tracker's own commit order whatever its dates say. It
  sorted every commit by committer date, so a real history with a rebased or
  cherry-picked commit (Express, 6,173 commits) recomposed into new commit
  ids, and the project root's first split then re-split almost all of it.
  Express now composes into itself, carved trackers included, in seconds.
- A split writes its notes in one batch instead of once per commit.
