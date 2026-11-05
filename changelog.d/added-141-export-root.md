- Export targets take `root = "<tracker>"`, exporting the view rooted at
  that tracker: its own files at the top
  ([#141](https://github.com/Spheroman/newgit/issues/141)).

  A fork such as Spheroman/t3code keeps its files at the repository root,
  so exporting the project's view would put them under the mount and never
  match the fork's ids. Rooted at a tracker that mounts no tracker in Git,
  the export is that tracker's own `main`, so the first export of an
  imported tracker is a no-op fast-forward of its upstream. A root that
  mounts a tracker in Git is refused, naming it; `root` and `trackers`
  cannot both be set. With `branches = true` each instance's branch of the
  tracker goes there, and `gh` routing follows.
