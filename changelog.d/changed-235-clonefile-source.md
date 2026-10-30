- On macOS, `spawn` clones a workspace's source from a template checkout
  with `clonefile` instead of running `git clone`
  ([#235](https://github.com/Spheroman/newgit/issues/235)).

  `git clone` spent almost all of its time checking files out: 2.7–3.1s
  per spawn on T3 Code's 28.6k files. newgit now keeps one clean checkout
  of the project beside the workspaces (`.source-template` under the
  workspace root), brings it to the instance's revision, and clones the
  whole tree in one call, in 0.67s. The first spawn builds the template
  and is slower once. Workspaces are set to `core.checkStat=minimal` and
  `core.trustctime=false`, which let `git status` trust the cloned files
  instead of re-hashing every one; the only edit Git then misses is one
  that keeps a file's size and has its mtime set to the exact second Git
  recorded, as `touch -r` can. Checkpoints hash every file, so they
  capture it regardless. Anything that stops the template from working,
  such as a workspace root on another volume, falls back to `git clone`,
  and `spawn` says why.
