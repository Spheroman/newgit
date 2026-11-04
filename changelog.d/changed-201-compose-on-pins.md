- A workspace's history is composed from pinned trackers: each root commit
  records the commit of every tracker in Git it mounts (a gitlink at
  `.newgit/pins/<name>`), and root's history is the view's shape, one root
  commit per view commit ([#201](https://github.com/Spheroman/newgit/issues/201)).

  Compose used to interleave every tracker's commits by committer date and
  recover their order from `Newgit-Split-Parent` trailers. That worked for
  trackers carved out of one project and broke for any history from
  elsewhere: an imported repository landed under newgit's own commits and
  every id after it changed. Now the parent says which commit of the child
  it holds, and nothing is inferred. Trackers store their files relative to
  their own root, so one tracker can be mounted by two parents, and an
  imported repository is stored as it is. A child's changes split from the
  same commit come back inline; history from anywhere else comes in as a
  `git subtree`-shaped merge, so `git log -- <path>`, `blame`, and `bisect`
  stay real through it. Split walks every parent, so a merge inside the
  work being split is kept rather than flattened, and a commit touching two
  trackers stays one commit wherever it is merged. The date interleave and
  `Newgit-Split-Parent` are gone.
