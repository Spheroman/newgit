- `newgit stop --evict` frees an idle instance's disk: it stops the
  instance, checkpoints it, and deletes its workspace, leaving only the
  marker at its path. `newgit start` brings the workspace back from that
  checkpoint, at the same path: uncommitted and staged work, untracked
  files, other local branches, tags, the stash, tracker content (encrypted
  ones decrypted here), renders, and installs refilled from the install
  store. It refuses, deleting nothing, when something would not come back:
  ignored files no tracker owns and no resource rebuilds, such as
  `target/` (listed; `--discard-ignored` evicts anyway), a merge or rebase
  in the middle, hand edits to a rendered file, an untracked repository,
  a process still working in the workspace, or a degraded checkpoint. An
  eviction or restore cut short finishes on the next `start`. T3 Code
  evicts a settled thread's workspace this way once its settle action
  finishes and no terminal still runs there, unless **Free disk when a
  newgit thread settles** is off in Settings → Threads.
