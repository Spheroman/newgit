- A tracker history too big for one push to the remote goes in steps
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Cloudflare Artifacts refuses any one push over 64 MiB, and an imported
  history passes that easily: Spheroman/t3code packs to 365 MiB, so
  publishing it was refused. When the remote refuses a push for its size,
  newgit now sends the history along its first-parent line in steps on a
  temporary `newgit-staging` branch, sized from the reported size and
  halved whenever a step is still too big, then makes the same leased push
  and deletes the staging branch.
