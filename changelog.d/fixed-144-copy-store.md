- An instance's first push of a large tracker no longer fails partway through
  the remote's copy ([#144](https://github.com/Spheroman/newgit/issues/144)).

  When Artifacts dropped the fetch, the copy stepped through bands in
  `trackers.git`, and another fetch there collided with it: `shallow file has
  changed since we read it`. A copy now has its own store, so a merge's check
  never sees a shallow history, and only one stepped fetch runs in a store at
  a time.
