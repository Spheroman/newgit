- `newgit tracker merge` on a shared secret tracker with
  `storage = "remote"` now moves its `main` on the remote too.

  A checkpoint's sync carries only such a tracker's pins and instance
  branches, and the merge moved `main` on this machine only, so a merged
  rev never reached anyone else. The merge now pushes it first, as the
  tracker repo's branch `main` (where a code tracker's `main` lives too),
  and fails whole if that push does. newgit.dev's landing page reads that
  branch by name, so publishing it is `newgit tracker merge site`.
