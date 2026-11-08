- Two merges at once, at different anchors, both land and leave `main`
  whole ([#201](https://github.com/Spheroman/newgit/issues/201)).
  - Commits that root already has below its head no longer count as a
    change touching root.
  - A merge that lands below root waits for root to roll up, then brings
    root's new head into the project root, so the next spawn has every
    landing and not just its own.
  - Two merges from one project root no longer fail on each other's notes
    or tracking refs.
