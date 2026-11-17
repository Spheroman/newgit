- Writing objects no longer fails now and then on macOS with `Invalid
  argument (os error 22)` while Git cleans up the same repository
  ([#475](https://github.com/Spheroman/newgit/issues/475)).

  `git repack -d`, which `gc` and the detached `maintenance run --auto`
  after every `git commit` run, removes each empty fan-out directory under
  `objects/`. When that `rmdir` lands just as a file is made in the
  directory, macOS answers `EINVAL` rather than `ENOENT`, which the
  in-process writer did not take for a removal (Git itself fails the same
  way). It now treats either as one, makes the directory again, and tries
  again, a few times over: an `EINVAL` counts only when the directory is
  gone or is a different one afterwards, so a real `EINVAL` still fails.
