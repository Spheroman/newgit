- A workspace's `git push` could put a commit purged from an export target
  back there, when that target was the project's own `origin`: the push is
  forwarded to `origin` as it is, with none of an export's checks
  ([#442](https://github.com/Spheroman/newgit/issues/442)). An export
  target may no longer be `origin` (compared however either URL is
  spelled, `insteadOf` included); reading the targets refuses it, saying
  why and what to do, and the forward refuses it again. The forward to
  `origin` also refuses a ref holding a commit a reset (`newgit pull
  --remote --force`) dropped that `origin` does not have yet. A plain
  export of `main` is now leased on what the target had when it was
  checked, so a purge resetting it to an ancestor in between is refused
  rather than fast-forwarded over; an instance's branch or a tag pushed
  onto what is there still is not, and that window is open.
