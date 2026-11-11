- `newgit serve` merges on the server instead of answering "merge again"
  ([#411](https://github.com/Spheroman/newgit/issues/411)).

  Before, a merge landed only onto the exact state it began from, so two
  merges begun from one base conflicted even at different trackers, and
  the second was told to merge again. Now a branch that did not move
  fast-forwards to the merger's own commits, ids and signatures kept, and
  one that moved is merged onto by the server: Git's merge (`merge-tree`)
  over the composed tree, split back into each tracker's history, with one
  merge commit in each tracker both sides moved (the merger's commit its
  second parent, as GitHub's merge button writes it). A file moved between
  trackers, or added under a directory another landing renamed, is
  followed. Only a real conflict is refused, naming each file.

  The rules are read at the state the merge lands on, so a maintainer
  removed while a merge ran does not land it. A change to the definitions
  is a barrier that bumps the project's epoch, and a merge prepared under
  another epoch starts again; a merge that loses the race three times gets
  a turn to itself. Composed objects live only in a scratch repository no
  one is served from; each tracker's store receives only that tracker's
  own commits. The CLI brings the landed state back when the server merged.
