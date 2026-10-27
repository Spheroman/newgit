- Two commands on one instance no longer run over each other
  ([#212](https://github.com/Spheroman/newgit/issues/212)).

  Nothing serialized them: two overlapping checkpoints minted the same id
  and the one saved last replaced the other, and an `action` saved back the
  whole record it had loaded before a parallel one changed it, so a failed
  `prepare` came back as ready. Every command that changes an instance now
  holds that instance's lock (`.newgit/local/locks/<slug>.lock`, an
  advisory file lock the kernel releases with the process, so a crash
  leaves nothing stale) from start to finish, and a second command on the
  same instance waits, saying so. A hook run under the lock sees
  `NEWGIT_LOCKED`, the locks it runs under; a `newgit` it starts that would
  need one of them refuses instead of waiting for its own ancestor. Records
  are also written to a temporary file, flushed, and renamed into place, so
  a reader — `status`, `spawn`, anything that lists instances — never
  parses one half-written, and a crash mid-write leaves the record it
  found.
