- Tracker lanes are Git commits in a bare repo at `.newgit/local/lanes.git`
  instead of directories under `.newgit/snapshots/`.

  A rev is a parentless commit with a fixed identity and date, so it is
  still content-addressed — identical content is the identical rev, now on
  any machine. The repo is separate from the store repo so lane content
  (often secrets) never reaches a `git push --mirror` or a workspace clone's
  object store, and `newgit cleanup` garbage-collects pruned revs at once.
  This is what lets a tracker sync to a remote.

  Upgrading: existing content under `.newgit/snapshots/` is not migrated.
  Re-seed each tracker (`newgit tracker capture <name> --from-store`) or
  re-capture it from an instance, then delete `.newgit/snapshots/`. Checkpoints recorded before
  the upgrade cannot restore tracker content.
