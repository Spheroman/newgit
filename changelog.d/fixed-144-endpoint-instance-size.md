- The Git endpoint's container is big enough for a large tracker
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  No instance type was set, so the container ran at Cloudflare's smallest
  size, and composing a project with Spheroman/t3code mounted died with
  `Network connection lost` after about five minutes. It now runs as
  `standard-1`: 4 GiB of memory, half a vCPU, and 8 GB of disk.
