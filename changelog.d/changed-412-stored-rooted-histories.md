- `newgit serve` stores each tracker's rooted history and extends it as
  things land ([#412](https://github.com/Spheroman/newgit/issues/412)).

  A view rooted at a tracker (`/<owner>/<project>/<path>.git`, and the
  project's own view at root) used to be composed per fetch, its whole
  history again each time anything landed. Now each tracker's view, as
  everyone who reads its store may read it (its own store's trackers and
  public ones mounted inside), is kept in its store and extended by
  composing only the new commits: after each landing in the background,
  and by any fetch that finds it behind, so a fetch never sees a view
  behind what landed. Its commit ids are exactly what composing gives.
  A reader who may read more than that (a private tracker mounted inside)
  is still composed for per fetch. A data directory from before this gets
  its stored histories at start, in the background and resumably, and any
  fetch before that composes what it needs itself.
