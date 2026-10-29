- Checking a built tree for its workspace path, before it is stored, now
  reads the tree in parallel
  ([#235](https://github.com/Spheroman/newgit/issues/235)).

  Every install that could be stored is scanned for the workspace's path
  first, and the scan read the whole tree on one thread: on T3 Code's
  3.8 GB of `node_modules` that took 17–34s, longer than the install it
  was checking. It now walks the tree on every core and searches with
  `memchr`, and takes 3.4–3.8s on the same tree, about what
  `rg -uuu -F` takes.
