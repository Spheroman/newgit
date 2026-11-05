- Publishing a tracker history too big for one push to the remote works
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Cloudflare Artifacts refuses any one push over 64 MiB, and drops some
  pushes that bring several large new files at once. Spheroman/t3code packs
  to 365 MiB, so publishing it was refused. When the remote refuses or drops
  a push, newgit now sends the history along its first-parent line in steps
  on a temporary `newgit-staging` branch. Steps are sized from what the
  remote reports, halved when one is refused, and grown back after one gets
  through. A single commit the remote still will not take has its large
  files sent ahead one per push. Then comes the same leased push, and the
  staging branches are deleted.
