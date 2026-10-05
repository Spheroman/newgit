- Fixed data loss when a code tracker was carved after an instance's work
  had been split: carving now covers every branch and refreshes every view,
  and split refuses rather than start a tracker from an empty tree.
- Remote checkpoint sync checks every commit it uploads for secret files,
  merges included, like a push does; deleting a secret file is never
  refused; split no longer refuses secrets on its own, so one gate decides.
- The project root's tracker branches follow its own branch (the one
  checked out at `newgit init`), not a feature branch checked out there; an
  instance may not be named `main`.
- A split survives Git collecting a removed instance's commits, and does
  less repeated work (one fetch, the tip checked first, recomposing only
  when the set of code trackers changes).
