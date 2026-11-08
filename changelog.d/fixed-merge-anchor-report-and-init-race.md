- `newgit merge` says where a change actually landed. It used to report
  `begin`'s anchor, which is always root because `begin` can't read the
  merge's commits yet.
- Two merges begun at once no longer fail with `cannot copy ...: File
  exists`. Both of their checks ran `git init` on the same new store in the
  remote's container. The store is now made beside it and renamed into
  place.
