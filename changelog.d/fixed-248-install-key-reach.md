- Binding a tracker that cannot change an install no longer rebuilds it
  ([#248](https://github.com/Spheroman/newgit/issues/248)).

  Since #153 the install store keyed a tree on the whole binding, and since
  #206 `tracker use`/`drop` rebuilt every install when the binding moved.
  So switching on a branch database (a secret tracker with `paths = []`)
  cost a full `npm ci` for an identical tree, `--use branch-db` and the
  default binding each installed once at spawn, and creating or renaming
  any tracker re-keyed every install.

  The key now holds only what of the binding can reach the install: which
  paths the dropped code trackers leave out of the checkout (what an
  unfiltered `npm ci` reads by finding members on disk), plus the bound
  tracker names and paths only when the install is told them through
  `{{bound…}}`, in its own definition or in any resource's exports or
  renders. `use`/`drop` and undo's `recompute` skip compare that same
  material, so an install the change cannot reach is kept and not listed.

  An install that reads a secret tracker's file (an `.npmrc`, say) should
  name it in `[identity] paths`. Store entries keyed under the old scheme
  stop matching, which costs one reinstall per key; `newgit cleanup`
  reclaims them.
