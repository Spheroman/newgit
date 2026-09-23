- A plain `git push` from a workspace now publishes to the project's real
  remote ([#96](https://github.com/Spheroman/newgit/issues/96)).

  A workspace's `origin` is the store, so a push used to land there
  silently: it moved the store's branch ref behind checkpoint's back,
  reached nothing anyone else could see, and reported success. Agents kept
  rediscovering this and hand-rolling the two hops. Now the store's side of
  the push checkpoints the instance (reason `push` — `undo` cannot take a
  push back, so that is the moment a checkpoint has to exist) and forwards
  the push to the store's own `origin`. The push succeeds exactly when that
  remote accepted it; a rejection there, or a store with no `origin`,
  refuses it. The route is per-workspace Git config pointing at a hook in
  `.newgit/local/hooks/`, so the store's own hooks are untouched. Existing
  workspaces pick it up at their next `newgit checkpoint`. `newgit run` also
  sets `GH_REPO` from the store's `origin`, so `newgit run -- gh pr create
  --head <branch>` reaches the real repository (without `--head`, `gh` still
  resolves the branch through the store and fails). `git fetch`/`git pull`
  in a workspace still read the store.
