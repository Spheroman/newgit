- `newgit start` checks the ports an instance claimed, and `newgit gc` no
  longer fails while a fetch deepens the tracker store
  ([#441](https://github.com/Spheroman/newgit/issues/441)).

  A port is claimed once, at spawn, and only other newgit processes
  respect it. Another program could take it while the instance was
  stopped, and the service then failed to bind it, or moved quietly to the
  next port (Vite does) while the rendered `.env` files and the exported
  `APP_URL` still named the old one. `start` now checks each port a
  resource claimed before the first thing it runs for it, its `resume`
  included, and for one something is listening on it claims a free port,
  writes it to the binding record, renders the exports and files again,
  and says so: a line on stderr, and `moved_ports` in `--json`. One that
  cannot find a free port fails that resource, naming the port. A port in
  TIME_WAIT from the resource's own stopped server is not taken for a
  listener, and a running dependent started with the old port is restarted
  with the new one, with a warning.

  `gc` died with "failed to run repack" when it met the `shallow.lock` a
  stepped fetch holds for each band. It now takes the stepped fetch's own
  lock first, saying so if it has to wait, and waits for the fetch to
  finish; when another `git gc` is running it tries again for about half
  a minute. The opening whole fetch into an already shallow store, which
  deepens it, now takes that lock too and skips Git's auto-maintenance, as
  each band does.
