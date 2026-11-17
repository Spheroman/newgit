- A branch exported to a mirror is the mirror's `main` plus its own
  commits, whichever of the two was exported first.

  A commit merged on a mirror comes back as a root commit naming the
  mirror's own. An instance started on the remote's `main` as the project
  root had fetched it (where a Claude Code worktree starts) was never told
  that commit was the remote's, and neither was any instance when `main`
  had been composed here before one of its trackers' commits arrived. Its
  split then made a second root commit of the same commit, without the
  signature, and the export composed that into a second commit on the
  mirror, with a second copy of everything above it. The pull request
  showed commits that were not its own, by `newgit@localhost`, and merging
  it would have put both copies in the mirror's `main`.

  A split now finds the remote's root commit for anything the remote's
  `main` holds, whether the workspace or the project root fetched it, and
  only counts `main` as composed while it holds every pin this machine
  has. An instance already built on a copy heals at its next push: its
  commits are split again onto the remote's root commits (the workspace's
  own commits do not change), and its branch on the mirror is replaced,
  only while the mirror holds exactly what newgit last pushed there, by a
  record newgit keeps in the project repo: a commit someone else pushed
  there is never thrown away, and a branch deleted there is pushed again.
  `newgit export <instance> --remote <target>` does that without a new
  commit, healing the instance's tracker branches too, and exports a branch
  again when its export failed. A force-push from the workspace replaces
  the branch there the same way, over that record only; with no record
  (another machine, or an export by an older newgit) the export is refused,
  saying so, while the push itself succeeds. `newgit remove` drops the
  record with its instance. `newgit export <instance> --remote <target>
  --force` is the one way to replace the branch there whatever it holds.
