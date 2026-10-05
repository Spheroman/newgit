- Each tracker's history is real Git history in a bare repo at
  `.newgit/local/trackers.git`, separate from the project repository:
  `main` (what new instances get), one branch per instance holding its
  captures as ordinary commits with a parent, author, and message, and pins
  keeping whatever bindings and checkpoints name. Capturing unchanged content
  makes no commit. Tracker content never reaches the project repository, a
  `git push --mirror`, or a workspace clone's object store, and
  `newgit cleanup` garbage-collects pruned revs at once.

  Upgrading: content under `.newgit/snapshots/` (and an interim
  `.newgit/local/lanes.git`) is not migrated. Re-seed each tracker
  (`newgit tracker capture <name> --from-store`) or re-capture it from an
  instance. Checkpoints recorded before the upgrade cannot restore tracker
  content.
