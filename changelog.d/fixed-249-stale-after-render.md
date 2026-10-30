- `tracker use` and `drop` no longer leave a running dev server on a
  rendered file they rewrote without saying so
  ([#249](https://github.com/Spheroman/newgit/issues/249)).

  Binding a tracker binds the resources it brings, and their `[[render]]`
  rewrites files such as `apps/admin/.env.local` that a process already
  running read when it started. That process kept the old port, and the
  output gave no hint. A resource can now declare `reads = [...]`, the
  files and directories its process reads; when a use or drop changes the
  bytes of one, the process is restarted, as `undo` restarts what it
  restores. A running process whose resource declares nothing is named in
  a warning with the command that restarts it. A re-render that writes the
  same bytes says nothing.
