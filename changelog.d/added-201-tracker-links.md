- One tracker in two roots: `link = "<owner>/<project>[/<path>]"` in a
  tracker's definition, written by `newgit tracker link <name> <home>
  --mount <path>`
  ([#201](https://github.com/Spheroman/newgit/issues/201)).

  A linked tracker is its home's: one history, one repo, one `main`, and
  the home's policy, judged by membership there. Each root that links it
  pins its own commit of it, and a change merged from any of them lands at
  the home and rolls up in all of them. See docs/design/trackers.md,
  *Links*, for what is clean and what is not yet.
