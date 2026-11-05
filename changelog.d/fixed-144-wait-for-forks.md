- A sync waits while Artifacts forks a large tracker for an instance
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Artifacts forks a large repository in the background and says
  `FORK_IN_PROGRESS` until it is done, which newgit.dev passed on as an
  error, so the first push from an instance that changed Spheroman/t3code
  failed. The remote now answers `202 {"forking": true}` while the fork
  runs, and the client asks again, saying once a minute that it is still
  forking.
