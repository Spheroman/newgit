- A tracker's definition can say `history = "pins"`
  ([#201](https://github.com/Spheroman/newgit/issues/201)). Its parent on
  the remote then records it as one snapshot of where it is now, without
  its commit messages. That fits things that merely sit together, like
  users under one root. The default, `commits`, keeps one commit per change.
- `newgit merge` merges tracker by tracker when an instance changed only
  pins in root. That keeps working when root keeps none of the instance's
  history, as under `pins`.
- The remote sweeps each project within the hour of a landing. It rolls up
  what nobody has read yet and clears the landing refs that merges leave
  behind.
- A merge sends its commits to the remote in parallel.
