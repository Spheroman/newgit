- A resource may not export any name starting with `NEWGIT_`
  ([#338](https://github.com/Spheroman/newgit/issues/338)).

  Only `NEWGIT_BRANCH`, `NEWGIT_WORKSPACE`, and `NEWGIT_LOCKED` were refused
  before, so an export named `NEWGIT_KEYS_URL` or `NEWGIT_IDENTITY` would
  have changed whose keys a `newgit` run inside `newgit run` encrypts to, or
  which key it decrypts with. `$NEWGIT_KEYS_URL` itself now takes only an
  `https://` base or a `file://` directory, never `http://`.
