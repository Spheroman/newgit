- `newgit tracker merge` on a shared secret tracker with
  `storage = "remote"` now moves its `main` on the remote too.

  A checkpoint's sync carries only such a tracker's pins and instance
  branches, and the merge moved `main` on this machine only, so the remote
  never had one: newgit.dev's landing page, which reads it, redirected to
  GitHub. The merge now pushes it first, as the tracker repo's branch
  `main` (where a code tracker's `main` lives too), and fails whole if that
  push does, so merging such a tracker now needs a login and the network.
  Publishing the page is `newgit tracker merge site`. Other machines don't
  read that `main` back yet; `tracker pull` there still takes their own.
