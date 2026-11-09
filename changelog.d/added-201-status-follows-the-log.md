- `newgit status` says what the remote hasn't written down yet: changes
  landed below a tracker that hasn't rolled up to them, and why its last
  roll-up failed
  ([#201](https://github.com/Spheroman/newgit/issues/201)). It also says
  which webhook pulls are waiting or failing, and which were refused or
  given up. It prints this only when there is something, and asks the
  remote for no more than three seconds. `newgit remote show` always shows
  it, along with the last webhook pull.
- A change merged on a GitHub mirror lands on the remote at its anchor, as
  `newgit merge` does, rather than always at root.
