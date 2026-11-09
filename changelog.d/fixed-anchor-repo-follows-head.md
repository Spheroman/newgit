- After a merge lands below root, the anchor tracker's repo `main` now
  follows its head on the remote, as root's always did. Before, it moved
  only when the merging client asked with the right old commit, so a
  client with a stale view could leave it behind, and the next merge fetched
  a stale `main`.
