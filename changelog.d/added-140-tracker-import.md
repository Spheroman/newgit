- `newgit tracker create <name> --code --path <path> --from <repository>
  [--from-branch <branch>]` brings another repository's history in as a
  code tracker ([#140](https://github.com/Spheroman/newgit/issues/140)).

  Every commit is rewritten with its tree under the path, every parent,
  author, date, and message kept, so the same outside history always gets
  the same ids. It joins the project root's branch through one merge on top
  of its tip, so nothing already there is rewritten, and `git log` and
  `git blame` under the path reach the imported commits. A note on each
  original (`refs/notes/newgit-imported`) names its rewrite. The import
  refuses a path that already has files, and history that adds a file a
  secret tracker owns.
