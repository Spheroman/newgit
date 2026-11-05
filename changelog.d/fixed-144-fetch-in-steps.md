- Fetching a tracker history too big for the remote to send at once works
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Cloudflare Artifacts fails a fetch of all of Spheroman/t3code (365 MiB)
  with an HTTP 500 (`expected 'packfile'`), so the remote could not check a
  merge that brought t3code in, and could not have served a clone of it.
  When the remote drops a fetch, newgit now fetches the tips and deepens
  band by band until nothing is shallow, halving a band the remote still
  drops; a store left shallow by an interrupted fetch carries on deepening
  on the next.
