- Whose a claimed port is gets decided by the process listening on it,
  not by the claim.

  `newgit ports --check` said "ok 3001 — claimed by `gamma`" while
  `beta`'s process held 3001, and failed on AirPlay, adb and an editor,
  which no instance claims (AirPlay twice, once per address family). It now
  judges only claimed ports: ok when the claimant's own process holds it or
  nothing does while its process is down, FAIL naming the holder's pid,
  directory and instance otherwise, `?` for a holder nothing ties to an
  instance on a resource newgit runs no process of (Docker's proxy), and a
  `warn` for a port an instance's process listens on that nothing claims.
  `ports` takes `--json`, with the same exit code. `status`, and so the end
  of `spawn`, `start` and `rebind`, names a claimed port's holder when it
  is another instance's process, instead of leaving a bind failure in a
  log, and no longer calls an instance up because somebody else answers on
  its port.
