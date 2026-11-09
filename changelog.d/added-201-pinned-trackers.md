- A tracker's definition can say `pinned = true`
  ([#201](https://github.com/Spheroman/newgit/issues/201)). Its parent then
  holds it where it is: a change to it never rolls up into the parent by
  itself. Moving its pin is a change to the parent, approved by the parent's
  maintainers, and rolling back is a revert. It's for what must not move on
  its own, like a private dependency or `/env/prod`.
  - `newgit status` says when a pinned tracker's own `main` has moved past
    its pin.
