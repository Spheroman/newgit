- Copying refs between the project repo, the tracker store, and a workspace
  is a ref move when the destination already holds the history, and a
  fetch only when it does not.

  A `git fetch` between two repos on one machine costs 25–40 ms of process
  start-up and negotiation even when there is nothing to transfer, and a
  `spawn` ran three, an `undo` two, one per tracker per landing. Now newgit
  runs Git's own completeness check and one `update-ref --stdin`, about
  3 ms, and lands every tracker's branch to the store in one call. Nothing
  borrows objects (no alternates): each repo still holds a real copy, so a
  deleted workspace or a `gc` elsewhere cannot take anything away.

  These copies also stop following tags. A project's tag used to land in
  `.newgit/local/trackers.git` and come back into the project on the next
  spawn after it was deleted.
