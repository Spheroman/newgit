- `newgit serve` lands concurrent merges about twice as fast, and
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
  and after run back to back), merges landed a second: `mixed` 1.4 to
  2.7 (median of five pairs), `same` 1.3 to about 2, `spread` 0.5–1.2
  to 1.1–2.1, and `overlap` 0.45 to 1.1. With 27 trackers, `overlap`
  went from 0.26 with 13 of 24 merges answered busy to 0.7–1.0 with none.
  The time a landing holds its project went from about 600–900 ms to
  about 300–500 ms.
