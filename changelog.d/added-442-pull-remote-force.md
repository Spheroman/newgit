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
  machine then refuses to carry them onward from the tracker's line: a
  merge, a send to the remote (a checkpoint's sync included), or a push to
  any export target that lacks them, the project's own view included,
  whose older root commits still pin them. An instance with nothing of its
  own on the old line follows on `newgit pull`; the reset refuses while the
  project root has work of its own on the old line, which it would
  discard. A later reset back onto dropped commits says so and stops
  refusing them, and a reset that fails with nothing moved leaves no
  record. It says what it cannot undo: the project's own history (so the
  remote's copy, its Git endpoint, `newgit clone`, and project tags) and
  copies others already fetched. Plain `pull --remote` now refuses a target
  rewritten since the last export, a rewind with no new commit included,
  and so does every push to it that would carry what the rewrite removed:
  `main`'s export (plain, `--dry-run`, and the one a push makes), an
  instance's branch, and a tag. Every push to a target goes through one
  check, so none can skip it. A branch deleted there, with a record of
  exporting to it, counts as a rewrite that removed everything, and a tag
  pushed with `main` is not pushed to a target `main`'s export was refused
  on. These name `--force` instead of merging back or advising `export
  --force`;
  plain `pull --remote` from a second target still holding a dropped commit
  is refused too. `export --remote --force` refuses, before pushing, to put
  back commits a rewrite removed, naming how many, unless `--republish` is
  given, and leases on the commit it checked. With no record of exporting
  there, or a record older than another machine's export, a rewrite is not
  detected ([#473](https://github.com/Spheroman/newgit/issues/473)); the
  refused push then names `pull --remote --force --dry-run` first. `--dry-run` moves nothing.
  Only a target rooted at a tracker for now: following a rewrite of the
  project's own view means rewriting the project's history, and its refusal
  says what to do instead.
