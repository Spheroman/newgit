- A tracker carved inside a mounted tracker in Git is now that tracker's
  child ([#201](https://github.com/Spheroman/newgit/issues/201)).

  `newgit tracker create vendored --git --path apps/t3code/.repos` defines
  it in `apps/t3code/.newgit/trackers/`, and `t3code`'s commits pin it, so a
  workspace can leave a directory out (`priority = -1`) while the parent's
  repository stays whole. An export with `root = "t3code"` ships what t3code
  mounts, where it used to be refused or lose those files: the only
  difference on the fork is the one definition file. `newgit pull --remote`
  splits what comes back, so an upstream change under a child's mount
  reaches the child rather than sitting hidden in the parent. The remote
  reads these definitions from the parent, whose maintainers govern them;
  `access` keeps any name it has a file for. The remote has to be deployed
  before a project merges one.

  This replaces the decision that such a tracker is a deeper mount of
  root's. A tracker carved that way before this release keeps working as
  one.
