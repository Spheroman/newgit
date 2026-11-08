- Tags reach the remote, and come back from it.

  The remote kept no tags: a release tagged on GitHub never reached a clone
  of newgit.dev, and `git push origin v1` from a linked project root was
  refused. A tag is now a name for one root commit on `main`, frozen with
  what it pins, as a checkpoint is.
  - `git push origin main v1.0` (or `--follow-tags`) in a linked project root
    tags that point on the remote, then on each export target, each at its
    own view of the commit. An annotated tag keeps its tagger and message;
    its signature, which covers one object, does not carry.
  - The Git endpoint serves tags as `refs/tags/<name>`, composed for each
    reader as branches are, so `git fetch` brings them.
  - A tag made on a mirror, as a GitHub release makes one, comes back
    through the webhook at the root commit it names. Our own export of a
    tag comes back too, and finds it made.
  - A tag never moves: the same tag again is a retry, another commit is
    refused. A tag on a commit `main` does not have yet is refused, saying
    to push `main` with it. Whoever may merge into root makes them.
  - A tag pushed from a workspace on a newgit remote used to vanish without
    a word; it is now refused, saying to merge and tag from the project
    root.

  The remote has to be deployed before a project root pushes a tag. Tags
  made on GitHub before this release come in when their webhook event is
  redelivered from the repository's *Recent Deliveries*.
