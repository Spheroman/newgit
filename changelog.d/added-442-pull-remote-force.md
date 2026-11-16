- `newgit pull --remote <target> --force` follows a force-pushed export
  target instead of merging with it
  ([#442](https://github.com/Spheroman/newgit/issues/442)). Merging, as
  plain `pull --remote` does, kept what the force-push removed in the
  tracker's history, so the next export pushed it back: a secret purged on
  GitHub was published again. Now the line of the tracker the target is
  rooted at is reset to the target's (as `git fetch` then `git reset --hard`
  would), the project root's pin moves to it, and `newgit merge main`
  replaces the old line on the remote instead of merging it back. It lists
  every commit it drops and every instance still holding one, and this
  machine refuses to merge, send, or export them again; an instance with
  nothing of its own on the old line follows on `newgit pull`. It says what
  it cannot undo: copies others already fetched. `--dry-run` moves nothing.
  Only a target rooted at a tracker for now: following a rewrite of the
  project's own view means rewriting the project's history.
