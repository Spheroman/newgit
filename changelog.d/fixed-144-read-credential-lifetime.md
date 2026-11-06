- A clone of a large tracker no longer loses its credential part way
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A stepped fetch of Spheroman/t3code outlasted the fifteen-minute read
  credential and failed. Read credentials now last an hour; each is still
  revoked with the login or instance it was minted for.
