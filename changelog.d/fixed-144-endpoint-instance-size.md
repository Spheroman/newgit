- A first fetch after a deploy no longer fails on a project with a large
  tracker ([#144](https://github.com/Spheroman/newgit/issues/144)).

  The first fetch composes the view inside `info/refs`, which took longer
  than the five minutes a request gets with no response started, so it died
  with `Network connection lost` and an HTTP 500. `info/refs` now starts its
  response at once and sends the body when the view is ready. The Git
  endpoint's container also runs as `standard-1` now: 4 GiB of memory, half a
  vCPU, and 8 GB of disk.
