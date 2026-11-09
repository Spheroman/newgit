- `git push` stops recomposing the remote's `main` when it has not moved,
  and stops routing exports when nothing takes branches.

  Whenever the project root held commits the remote had not merged, every
  push composed all of `main` twice, once in the checkpoint's sync and once
  in the push. Now the push reads where `main` is from the project record
  it already fetched, and composes (and fetches trackers) only when that
  root has moved since the last composition. Export routing returns at
  once when no target takes branches, and a push routes once instead of
  twice. `spawn --from-checkpoint` fetches a fork's branch and the
  checkpoint in one fetch. On the newgit project with `t3code` bound, a
  push to a local `newgit serve` went from about 5 s to 2.4 s.
