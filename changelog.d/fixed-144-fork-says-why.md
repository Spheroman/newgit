- An instance's fork that Artifacts fails says why, and one that finished
  anyway is taken
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Forking Spheroman/t3code for an instance failed on newgit.dev with only
  "internal error". The fork now answers 502 with Artifacts' error code and
  message, unless the fork exists by then, which a fork of a large
  repository that outlasted its request can.
