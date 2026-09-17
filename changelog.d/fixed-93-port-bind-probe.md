- The port allocator's bind probe no longer reports a port bindable when a
  Docker-published container port already holds it
  ([#93](https://github.com/Spheroman/newgit/issues/93)).

  It checked only `127.0.0.1`, using `std::net::TcpListener::bind`, which
  sets `SO_REUSEADDR` on by default on Unix. That let a loopback bind
  succeed even when something else — a Docker Desktop container publishing
  to `0.0.0.0`, in the reported case — already held the port on the
  wildcard address, so `spawn` allocated a port it could not actually use.
  Since allocation is permanent for the life of an instance, the bad value
  did not get retried; it just failed later, inside a resource's prepare
  log. The probe now binds, with `SO_REUSEADDR` off, on `127.0.0.1`,
  `0.0.0.0`, `::1`, and `::`, and only reports a port bindable if all four
  are free. An address family the OS doesn't support at all is skipped
  rather than counted against the port. If the probe can't positively
  confirm an address is free, it now declines to promise the port rather
  than claim it.

  A side effect worth knowing about: the old loopback-only probe, with
  `SO_REUSEADDR` on, could not see another process's simultaneous probe of
  the same port either — so two `newgit spawn` runs racing each other could
  both be handed the same port. The stricter probe closes that gap too:
  concurrent allocators now correctly see each other's in-flight claims and
  scan past them instead of colliding.
