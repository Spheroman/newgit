- A linked project root's `origin` is the remote's Git endpoint, as a
  workspace's is
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A mirror that withholds a tracker has ids of its own, so `git pull` from
  GitHub would bring in commits that are not the project's. `newgit remote
  add` sets this, and a project linked earlier moves the next time newgit
  syncs its root. Each `branches = true` target becomes a remote for `gh`.

  `git push` there publishes: pushing the project root's branch merges it
  into `main` and exports to each target whose trackers changed, each
  getting only its own. It used to go straight to GitHub, which would have
  carried every tracker. Any other branch is refused.
