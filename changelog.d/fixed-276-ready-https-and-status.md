- A `[ready] http` check of an `https://` URL trusts what this machine
  trusts, so a local certificate from `mkcert -install` passes; one the
  machine does not trust says so instead of reading like a server that is
  down. `status` runs an instance's checks side by side, and `status
  <instance>` runs only that instance's: 30 instances with three
  one-second checks each went from 6.5 s to 2.8 s, and one of them from
  6.5 s to 1.1 s ([#276](https://github.com/Spheroman/newgit/issues/276)).
