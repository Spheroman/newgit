- `newgit tracker use` puts a secret tracker's content in the workspace
  when the instance has never bound it, as `newgit spawn --use` does.

  A tracker dropped at spawn, because another tracker filled its role, has
  no content of its own in the instance, and `use` restored only content
  the instance held. So it bound the tracker with none of its files, and a
  resource the tracker brings failed: its render had nothing to land in.
  `use` now checks out the tracker's main first, records it as the
  binding's revision, and the resources it brings come up ready, rendered,
  in the one command. If you worked around it with `newgit tracker pull`
  and `newgit action <resource>.prepare`, you no longer need to.
