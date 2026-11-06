- `newgit spawn` no longer checks out what the binding leaves out.

  It wrote every file and then deleted the ones the instance does not bind,
  so leaving a tracker out made a spawn slower than bringing it. The sparse
  patterns now go in before the first checkout, and on macOS each binding
  has a template of its own beside `.source-template`. `newgit cleanup`
  removes a template no live binding uses.
