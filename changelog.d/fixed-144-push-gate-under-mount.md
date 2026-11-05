- Publishing an imported tracker no longer mistakes its files for those of
  a tracker outside Git
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A tracker stores its files at its own root, so an imported t3code's
  `AGENTS.md` is `apps/t3code/AGENTS.md` in the view. The push gate judged
  it as the `AGENTS.md` agent notes own at the top and refused
  `newgit merge main`. It now judges a mounted tracker's commits under its
  mount, as the import already did.
