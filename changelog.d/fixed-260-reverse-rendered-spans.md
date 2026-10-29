- `tracker capture` no longer rewrites text you typed that happens to equal
  a rendered value
  ([#260](https://github.com/Spheroman/newgit/issues/260)).

  Capture reversed every occurrence of a rendered value, so a
  `MIRROR=127.0.0.1:18002` line written by hand came back as `:8000`. The
  tracker then held the `find` twice, and every instance spawned from it
  failed to render until someone fixed the lane by hand. Capture now
  reverses only the spans the render wrote, recomputed from committed
  content. Where it cannot tell which occurrence it wrote, or the captured
  content would no longer render, `tracker capture` refuses and names the
  lines instead of recording a lane nobody can use.
