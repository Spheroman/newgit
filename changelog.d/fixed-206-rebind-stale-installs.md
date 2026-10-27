- Changing a live instance's binding rebuilds the installs keyed on it
  ([#206](https://github.com/Spheroman/newgit/issues/206)).

  Since #153 the install store keys a tree on the trackers its instance
  binds, because a shared install builds the bound members, named by
  `{{bound.paths}}` or found on disk. Two paths changed the binding of a
  live instance and left such a tree as it was. `tracker use` and `drop`
  only started and stopped the resources the tracker brings, and a shared
  install is brought by `root`, which is always bound, so an instance that
  bound `b` kept the tree built without it until someone ran `prepare` by
  hand. And undo's `recompute` skip compared the checkpoint's identity hash
  with the pre-undo one, which the binding is not part of, so an undo that
  restored a different binding kept the tree built for the one it replaced
  and reported `skipped: identity unchanged`.

  `tracker use` and `drop` now rebuild every bound install that holds a
  tree once the binding has moved, the same way a spawn builds it: filled
  from the store when another instance built that binding already, else by
  running its `prepare` and publishing the result. The command reports each
  rebuild; one that fails leaves the resource `failed`, with `newgit action
  <resource>.prepare` as the retry. A long-running action that was up on
  the old tree is restarted on the new one. Undo's skip now requires the
  binding it restored to be the one the instance had, so it compares the
  inputs the store key does; an undo that drops or brings back a tracker
  rebuilds, through the store first.
