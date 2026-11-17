- `newgit serve` brings in tags made on a GitHub mirror
  ([#453](https://github.com/Spheroman/newgit/issues/453)).

  A release tagged on GitHub now appears on the remote, as it does on the
  Worker: without this, moving a remote to `serve` would have lost it. The
  tag's event is queued behind any pull of the same mirror, so a pull
  request merged and then tagged is on `main` when its tag is looked for.
  Nothing lands: the root commit whose view is the tagged commit is found,
  and the tag is made there the way a tag pushed from the project root is,
  with an annotated tag's message and tagger. A tag made in the project
  root at a root commit that changed only what the mirror does not ship
  comes back as there already. A tag never moves, so one that exists at
  another commit is refused, and one naming a commit that
  is not on `main` yet is refused until it lands and the event is sent
  again; `newgit status` says both. Only for a mirror of the project's own
  history: a rooted target's tags are its tracker's.
