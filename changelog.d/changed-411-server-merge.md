- `newgit serve` merges on the server instead of answering "merge again"
  ([#411](https://github.com/Spheroman/newgit/issues/411)).

  Before, a merge landed only onto the exact state it began from, so two
  merges begun from one base conflicted even at different trackers, and
  the second was told to merge again. Now a branch that did not move
  fast-forwards to the merger's own commits, ids and signatures kept. One
  that moved since, on other paths, gets the merger's work laid onto it
  without running Git's merge: a new commit in each tracker both sides
  moved (root, and the parents along the mount chain), the merger's commit
  its second parent. One that moved on the same paths is merged by Git
  (`merge-tree`) over the composed tree and split back into each tracker's
  history. A file moved between trackers, or added under a directory
  another landing renamed, is followed. A landing that empties a tracker,
  adds or drops one, or changes a definition always goes through Git's
  merge, since a whole directory of the view may have moved. File names
  are kept as the bytes Git stores, so a name that is not UTF-8 lands
  like any other. Only a real conflict is refused, naming each file the
  merger may read and only that there are others.

  What lands is authorized at the state it lands on, membership read again
  at the last moment, so a merge that would carry the merger's edit into a
  tracker they may not change is refused, naming it. Nothing is carried
  into a tracker some may read who could not read where it was, unless
  someone chose to move it there. The base is
  what the server reads from root's commit, never a list the client sent.
  A change to the definitions on `main` is a barrier that bumps the
  project's epoch; a merge that loses its recheck three times takes a turn
  on the paths it changes, and merges on other paths land beside it.
  Composed objects live only in a scratch repository no one is served
  from, and an attempt that lost leaves nothing waiting. `newgit merge`
  says when the server merged, and with which commit.
