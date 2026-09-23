- A checkpoint no longer copies the workspace's tags into the store.

  Checkpoint fetches the workspace's state into the store, and a fetch
  follows tags by default, so every tag an agent created in a workspace
  appeared in the store as a side effect. Found because a `git push` of
  that tag was then refused: the store already had it.
