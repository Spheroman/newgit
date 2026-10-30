- Capture no longer follows a symlink at a tracked path
  ([#258](https://github.com/Spheroman/newgit/issues/258)).

  A link at a tracked path was read through, so `tracker capture` and the
  capture inside `checkpoint` stored the target's contents — even a file
  from outside the workspace — in a lane that may sync to a remote. Anything
  that could plant a link (a script, a tool, a force-added path) could copy
  any file you can read into the lane, silently. Now a link that stays
  inside the workspace is stored as a link (mode `120000`), as Git stores
  one, and checked out as one; a link that leaves the workspace, or a
  tracked path under a symlinked directory, is refused with the path and
  its target. Checkout likewise refuses a lane rev holding a link out of
  the workspace, whoever wrote it. The safety capture, `tracker drop`,
  `--from-store`, deposits, install keys, and export follow the same rule.
  If a capture now fails on a link you meant, replace it with the file or
  with a relative link inside the workspace.
