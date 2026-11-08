- `newgit merge` sends its commits to the remote before the merge begins
  ([#201](https://github.com/Spheroman/newgit/issues/201)).
  - Each commit goes as a thin pack, and the remote pushes it to its landing
    ref itself.
  - The remote can therefore read the commits when the merge begins. It
    asks only the maintainers of what the merge touches, so a change to
    `t3code` no longer needs root's approval.
  - It hands out no write passwords: the remote moves every ref.
  - A pack over 90 MiB, or an older remote, takes the old path.
