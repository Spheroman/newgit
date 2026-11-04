- `newgit tracker migrate [--dry-run]` moves a project laid out before
  mounts onto roots and mounts; `newgit tracker repair` is removed
  ([#201](https://github.com/Spheroman/newgit/issues/201)).

  `project`, `access`, and every other tracker in Git fold back into root,
  whose history becomes the view's history commit for commit, and one
  commit at the tip of `main` mounts them again (`access` at
  `.newgit/trackers`, the rest at the paths they owned). Every branch and
  checkpoint is recomposed in a fresh repository first, and nothing moves
  unless each comes out with the same id, signatures included. Old tracker
  refs are kept under `refs/newgit-premigration/`. On the remote it
  force-pushes every tracker repository, so run it once, after upgrading.
  `tracker repair` rewrote the trailers of commits split by an older
  engine; that engine is gone, and the migration covers what it fixed.
