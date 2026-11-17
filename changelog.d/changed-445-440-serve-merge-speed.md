- `newgit serve` lands concurrent merges two to three times as fast, and
  overlapping merges in a project with many trackers no longer time out
  ([#445](https://github.com/Spheroman/newgit/issues/445),
  [#440](https://github.com/Spheroman/newgit/issues/440)).

  A merge spawned about 54 `git` processes, and most of the ones that
  mattered ran while the project's landings were held, so merges landed
  one after another at about two a second. Now objects are written in
  process as loose objects in Git's own format (synced the way Git's
  `core.fsyncMethod=batch` does, one device flush for a batch), the
  object format is asked once per repository, and one reader per
  repository serves a whole attempt: what changed, whether a commit is
  there, ancestry, and whether a merge changes the definitions are read
  in process. A merge's commits are copied into their store in process
  rather than by `git fetch` (a copy of more than a few hundred objects
  is still Git's), and whether a store is for its audience is asked once,
  not on every request. About 27 processes a merge are left, most outside
  the hold.

  Merges whose changes overlap compose each tracker's view once per
  finish instead of once per view and attempt, in one scratch repository
  kept across attempts, and split reuses those views.

  Measured with `serve_server_merge_bench.rs` (24 merges from one base,
  finished at once, debug build, on a shared and busy machine, before
  and after run back to back), merges landed a second: `mixed` 1.2–1.4
  to 2.9–3.9, `same` 1.4 to 2.3, `spread` 1.0 to 2.7, and `overlap` 0.54
  (2 of 24 answered busy) to 1.3. With 27 trackers, `overlap` went from
  0.32 with 11 of 24 merges answered busy to 0.76 with none. The time a
  landing holds its project went from about 650–900 ms to 250–380 ms.

  Objects newgit writes in process into a repository are synced when
  that repository's config would have Git sync a loose object
  (`core.fsync`, or the older `core.fsyncObjectFiles`, read as Git reads
  them), as Git's `hash-object` did: a store's (stored rooted histories
  included) are synced, a scratch one's are not. How they are synced is
  always Git's `batch` method, whatever `core.fsyncMethod` says. Each goes
  into place only after everything it names, so a crash part way never
  leaves a store with an object whose tree or parents are missing, and a
  write of thousands keeps one file open at a time.
