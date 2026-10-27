- A tracker definition can record where a history from outside the project
  joined it, `imported = { anchor = "<view commit>", tip = "<commit>" }`,
  and composing brings that history in through one merge on the anchor
  instead of interleaving it by date
  ([#139](https://github.com/Spheroman/newgit/issues/139)).

  Adding such a tracker appends exactly one commit to every view and
  rewrites none: the project's commit ids stay the same everywhere, and
  `git log` and `git blame` under the tracker's paths reach through the
  merge into the imported history. A branch forked before the import leaves
  the tracker out until it merges `main`, and `newgit tracker use` there
  says so. Writing the anchor is the import's job (#140).
