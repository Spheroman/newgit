- `newgit tracker repair [--dry-run]` rewrites code tracker commits split
  before newgit kept each original's message ending and signature, so a
  clone from newgit.dev gets the project's own commit ids again.

  It moves every code tracker branch here and, on the remote, each
  branch's list, the way a merge does: a force push of tracker history.
  Instance forks and checkpoints there keep the commits they were made
  with. Run it once after upgrading; running it again changes nothing.
