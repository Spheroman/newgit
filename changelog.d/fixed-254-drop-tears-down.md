- Dropping a tracker takes down the resources it brings, and `remove`
  tears down what a drop left behind
  ([#254](https://github.com/Spheroman/newgit/issues/254)).

  A drop, or a `tracker use` switching a role, used to stop supervised
  processes and forget the rest: a Supabase stack or Compose project a
  dropped tracker brought kept running, and `remove` only cleaned up what
  was still bound, so it leaked them for good. A drop now *parks* each
  resource it takes away, the way `newgit stop` takes one down: its process
  is signalled and its `suspend` runs (or its `[cleanup]`, when it has no
  `suspend`), and the output names the hook that ran, or why none could.
  `remove` and `cleanup` run `[cleanup]` for parked bindings too.
