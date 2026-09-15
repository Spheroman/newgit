- Add `newgit start [instance]`, a `start_after` key, and a `[ready]`
  probe: a runtime edge that orders `start` and gates it on readiness, not
  just liveness
  ([#54](https://github.com/Spheroman/newgit/issues/54)).

  `depends_on` orders `prepare`. Nothing ordered `start` at all — `start`
  and `stop` were action names invoked by hand, one at a time, in whatever
  order a developer or agent remembered. "`functions` needs `supabase`
  actually running to serve against" had no way to be said, let alone
  enforced.

  `newgit start` walks an instance's resources in dependency order and
  brings up every `long_running` `start`, reporting each one it looked at
  rather than only the ones that had something to do. `start_after` is the
  new key: a subset of `depends_on` naming which of those edges also gate
  `start` — validated to be a subset rather than an independent list, so
  the two cannot drift apart, and so `start_after` reuses `depends_on`'s
  bind/lifecycle order instead of needing a third one of its own.

  "Waits on" means running *and ready*, not just running. A process that has
  been exec'd is not the same as one answering on its port, and ordering
  `start` commands without checking that would only relocate the race, not
  remove it. `[ready]` is a declared probe — `command`, `tcp`, or `http`
  against an allocated port — with its own timeout and poll interval. It
  is optional: a resource with no `[ready]` is only ever confirmed *alive*,
  and `newgit start` reports it as exactly that, in those words, rather
  than silently calling "the process exists" the same thing as "ready."
