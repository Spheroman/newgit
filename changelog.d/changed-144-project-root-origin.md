- A linked project root's `origin` is the remote's Git endpoint, as a
  workspace's is
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A mirror that withholds a tracker has ids of its own, so `git pull` from
  GitHub would bring in commits that are not the project's. `newgit remote
  add` sets this, and a project linked earlier moves the next time newgit
  syncs its root. `git push` there is refused, and `newgit merge main`
  publishes; each `branches = true` target becomes a remote for `gh`.
