- `newgit tracker create <name> --code --mount <dir> --from <repository>
  [--from-branch <branch>]` brings another repository's history in as a
  code tracker ([#140](https://github.com/Spheroman/newgit/issues/140)).

  The fetched branch becomes the tracker's history unchanged, ids and
  signatures intact: the tracker is stored at its own root, and its
  definition's `mount` says where the workspace shows it. It joins the
  project root's branch through one merge on top of its tip, so nothing
  already there is rewritten, and `git log` and `git blame` under the
  directory reach the imported commits. The import refuses a directory that
  already has files, and history that adds a file a secret tracker owns
  there.
