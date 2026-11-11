- `newgit serve` commits every landing to a landing log first, and puts
  the rest in place from it
  ([#410](https://github.com/Spheroman/newgit/issues/410)).

  Before, a merge wrote its new list to SQLite and then moved each
  repository's refs one by one, and a tracker changing audience moved
  between stores as a side effect of whichever request reached it next. A
  crash in between, or a move that failed part way, could leave readers
  seeing half a landing, or a widening's history readable by its new
  audience although the landing never happened. Now each landing is one
  commit in `<data>/landing.git`, and its place there is its stamp. Refs
  (one transaction per store), SQLite (one transaction), and store moves
  follow it, are repaired from it when the server starts, and nothing is
  shown until all of it is in place. Landings that arrive together are
  written as one batch, each still its own entry: about 280 a second on
  one machine for the simplest landing (one ref, one store, no objects;
  merges are slower). A branch's list at any earlier landing in place is
  `GET …/branches/<branch>?at=<stamp>`. An existing data directory gets a
  log that starts from what its database says; a log that is damaged, or
  missing once landings were put in place from it, stops the server.

  Nothing is written down that cannot be put in place: every ref change
  records its old value and is applied by compare-and-swap, every commit
  is checked to be in its store, and every project is an incarnation, so a
  landing never acts on a project deleted and made again under its name. A
  landing that is written down and still cannot be put in place answers
  503 with its real cause on its projects only, is tried again at each
  landing there and at each start, and can be given up with
  `newgit serve --void-landing <stamp>`, which appends a compensating
  landing.
