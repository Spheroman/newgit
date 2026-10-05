- The root tracker's history is copied into the tracker store at
  `newgit init` instead of borrowing the project repository's objects, so
  routine `git gc` in the project repository can never delete it.
