- `newgit stop --evict` frees an idle instance's disk: it stops the
  instance, checkpoints it, and deletes its workspace, leaving only the
  marker at its path. `newgit start` brings the workspace back from that
  checkpoint, at the same path, with uncommitted work, tracker content, and
  rebuilt installs. Ignored files that no tracker owns and no resource
  rebuilds, such as `target/`, are not kept, and the eviction lists them.
  T3 Code evicts a settled thread's workspace this way once its settle
  action finishes and no terminal still runs there, unless **Free disk
  when a newgit thread settles** is off in Settings → Threads.
