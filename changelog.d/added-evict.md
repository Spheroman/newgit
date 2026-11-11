- `newgit stop --evict` frees an idle instance's disk: it stops the
  instance, checkpoints it, and deletes its workspace, leaving only the
  marker at its path. `newgit start` brings the workspace back from that
  checkpoint, into the same directory: uncommitted and staged work,
  untracked files, other local branches, tags, the stash, notes, other
  remotes' refs, local Git config, hooks, file modes, empty directories,
  tracker content (encrypted ones decrypted here), renders, and installs
  refilled from the install store. It refuses, deleting nothing, when
  something would not come back: ignored files no tracker owns and no
  resource rebuilds (listed, a directory with its file count;
  `--discard-ignored` evicts anyway), a merge or rebase in the middle,
  hand edits to a rendered file, an untracked repository, an initialized
  submodule, a linked worktree, a process still in the workspace (the
  shell it was run from included), or a degraded checkpoint. An eviction
  or restore that fails or is cut short never deletes work it did not
  write: a killed restore's leftovers are moved aside, kept. This
  repository declares `target/` as a resource (`rust-target`), so a plain
  `--evict` frees a Rust workspace. T3 Code evicts a settled thread's
  workspace this way once its settle action finishes and no terminal still
  runs there, unless **Free disk when a newgit thread settles** is off in
  Settings → Threads.
