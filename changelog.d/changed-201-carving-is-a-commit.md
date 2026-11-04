- `newgit tracker create <name> --git --path <path>` is one commit,
  `Create tracker `<name>` at <path>`, that moves the files into the new
  tracker and pins its first commit; history before it stays in root
  ([#201](https://github.com/Spheroman/newgit/issues/201)).

  Carving used to split the paths' history out of root, commit by commit,
  and recompose every view. That rewrote tracker history each time and
  needed every branch of root carved too. A carve commit rewrites nothing
  and changes no id. The cost: rooted at the new tracker alone, its history
  starts at the carve. Carving with history can come back as an explicit
  rewrite if a case needs it.
