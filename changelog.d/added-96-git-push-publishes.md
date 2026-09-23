- A workspace's `origin` now behaves like the project's real remote, and a
  plain `git push` from it publishes
  ([#96](https://github.com/Spheroman/newgit/issues/96)).

  A workspace is cloned from the store, so its `origin` was a local path: a
  push landed silently in the store (moving the branch ref behind
  checkpoint's back, reaching nothing anyone could see, and reporting
  success), `git pull` read the store, and `gh` could not find the
  repository. Agents kept rediscovering this and hand-rolling the two hops.

  Now `origin` fetches from the store's own `origin`, so `git pull` and
  `gh pr create` (no flags) work as in any clone. Pushes go to an outbox in
  `.newgit/local/publish/`, kept identical to the real remote, whose hook
  checkpoints the instance (reason `push` — `undo` cannot take a push back)
  and forwards the push. It succeeds exactly when the real remote accepted
  it; a store with no `origin` refuses it. Fast-forward and
  `--force-with-lease` are judged against the real remote, not the store.
  Existing workspaces pick up the route at their next `newgit checkpoint`.
