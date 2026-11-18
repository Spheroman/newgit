- `newgit resource rebind` keeps the ports a resource holds, and a
  definition deleted by hand no longer leaves its process running
  ([#85](https://github.com/Spheroman/newgit/issues/85)).

  A rebind allocated every port again, and right after `newgit stop` the
  old one was often not free yet, so the port moved under every file and
  export that named it. It now keeps each port the definition still
  declares and allocates only new ones; a resource a tracker brings back
  after a drop does the same, rather than starting over when one port was
  added. A definition file deleted while its process ran left that process
  out of reach of `stop` and `remove`, still holding its port after the
  instance was gone. Both now stop it, and `resource rebind` on a resource
  with no definition releases its binding (its `[cleanup]` cannot run).
