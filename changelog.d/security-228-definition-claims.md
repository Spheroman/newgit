- A tracker handed one definition can no longer take `access`'s and lock
  the project's owner out
  ([#228](https://github.com/Spheroman/newgit/issues/228)).

  The remote gave a definition file to whichever tracker claimed it most
  specifically, and a claim is a change only the claimant's governors
  approve. So a tracker handed `web`'s definition could make `web` claim
  `access.toml`, write an `access` maintained by them alone, and take
  every definition in the project, `root`'s included. Governance now only
  delegates downward: a claim on `<T>.toml` counts only when the claiming
  tracker's own definition is held by whoever holds `<T>.toml`,
  `access.toml` is always `access`'s, and no tracker holds its own
  definition. The remote ignores any other claim. The client refuses a
  definition claiming `access.toml` or its own, which `newgit` used to
  accept; move such a claim to another tracker.
