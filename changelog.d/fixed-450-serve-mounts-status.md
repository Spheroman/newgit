- `newgit serve` refuses a landing on `main` that defines a tracker at a
  mount the client refuses to load (absolute, `..`, empty), with the
  client's own message, and admins can read background failures at
  `GET /v1/serve/status`
  ([#450](https://github.com/Spheroman/newgit/issues/450)).

  Serve used to only normalize the mount, so `mount = "/pkgs/a"` meant
  `pkgs/a` to it and nothing to the client: the two disagreed about where
  a tracker sat and what it owned. Both now use one check
  (`newgit_core::tracker::mount_problem`). A stored rooted history that
  failed to extend was only in the server's log; the status route also
  lists stuck landings.
