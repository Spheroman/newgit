- A merge whose finish failed after root's ref moved finishes on the next
  `newgit merge`
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A timeout or a 502 between pushing root's commit and the remote recording
  the merge left root's ref ahead of the index, and every later merge was
  refused as "another merge is landing". When root's ref already holds this
  merge's own commit, the merge now finishes it. The calls where the remote
  runs Git over the merge's trackers wait up to 15 minutes instead of 60
  seconds, since the first look at a large history takes minutes, and a
  check that fails says why.
