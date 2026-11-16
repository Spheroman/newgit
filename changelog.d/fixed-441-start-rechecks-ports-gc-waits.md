- `newgit start` checks the ports an instance claimed, and `newgit gc` no
  longer fails while a fetch deepens the tracker store
  ([#441](https://github.com/Spheroman/newgit/issues/441)).

  A port is claimed once, at spawn, and only other newgit processes
  respect it. Another program could take it while the instance was
  stopped, and the service then failed to bind it, or moved quietly to the
  next port (Vite does) while the rendered `.env` files and the exported
  `APP_URL` still named the old one. `start` now probes each claimed port
  before it starts a resource, and for one another program holds it claims
  a free port, writes it to the binding record, renders the exports and
  files again, and says so: a line on stderr, and `moved_ports` in
  `--json`. One that cannot find a free port fails that resource, naming
  the port.

  `gc` died with "failed to run repack" when it met the `shallow.lock` a
  stepped fetch holds for each band. It now takes the stepped fetch's own
  lock first, so it waits for the fetch to finish. The opening whole fetch
  of a stepped fetch into an already shallow store also skips Git's
  auto-maintenance now, as each band does.
