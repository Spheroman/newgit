- `newgit merge` lands a change at its anchor: the lowest tracker covering
  everything it touched
  ([#201](https://github.com/Spheroman/newgit/issues/201)).
  - A merge that only changes `t3code` no longer moves root. Root rolls up
    to it afterwards, on the remote, keeping the merge's own commit when
    nothing landed between.
  - Two merges at different anchors no longer wait on each other.
  - Approving a change is up to the maintainers of the trackers it touches.
    Root's maintainers are no longer asked about a change below root.
  - Every read of a branch rolls up first. If root can't roll up, the read
    serves root's last commit and marks the branch `behind`.
  - `newgit merge` says where the change landed when that's below root.
