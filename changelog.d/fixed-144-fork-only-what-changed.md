- An instance's first push no longer copies a tracker it never touched
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A tracker counted as changed whenever its commit was not `main`'s, so an
  instance behind `main` forked whatever `main` had moved since it spawned,
  which took nine minutes for Spheroman/t3code. Now a tracker still at the
  commit the instance last shared with `main` has nothing to send.
