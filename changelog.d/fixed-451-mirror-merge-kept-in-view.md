- A pull request merged on a mirror from an instance's branch is no longer
  dropped from the view exported back to that mirror
  ([#451](https://github.com/Spheroman/newgit/issues/451)).

  When the instance also had work in a tracker the mirror does not ship,
  the merge commit's two sides differed in that tracker's pin, and a view
  that leaves the tracker out dropped the merge as one that joined
  nothing. But the mirror made that merge and still holds it, so from then
  on the mirror had a commit `main` never showed it again: `newgit export`
  and the server's export both said the mirror was ahead, `newgit pull
  --remote` brought in nothing new, and only `--force` got past it. A
  merge that composes back to the very commit it was split from now stays.
  Views composed before this keep their old ids until composed again.
