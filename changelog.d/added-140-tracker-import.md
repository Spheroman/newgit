- `newgit tracker create <name> --git --mount <dir> --from <repo>
  [--from-branch <branch>]` imports another repository's history unchanged
  as a tracker in Git ([#140](https://github.com/Spheroman/newgit/issues/140)).

  A tracker is stored relative to its own root, so the fetched branch is
  used as the tracker's `main` as it is: every id, signature, and merge
  stays what upstream has. One commit in the project root adds the
  definition, the mount, and the pin, and composes as a merge that brings
  the history in under the mount, so `git log` and `git blame` work through
  it. It refuses, leaving nothing behind, when the mount already has files
  or the history adds a file a tracker outside Git owns there, and says when
  the import has a `.gitmodules`, which Git does not read under a mount.
  `newgit merge main` publishes it, as for any new tracker.
