- A checkpoint no longer captures an instance's rendered values into a
  secret tracker's lane, and neither does the safety capture `tracker
  pull` takes before it re-binds. Like `tracker capture` and `tracker
  checkout`, both now reverse the `[[render]]` substitutions in files the
  tracker owns for the capture, so the lane gets the committed values and
  any edit made beside them.

  The lane is shared by every instance, so an instance's allocated port
  did not stay with it: once `tracker merge` made such a rev the lane's
  `main`, each instance spawned after that got that port in the file and
  failed to render it. The checkpoint also records the rev as the file's
  committed content, which the render's `find` no longer matched, so
  `undo` to it warned that the re-render failed.

  A rendered file holding exactly its committed content, which is what a
  tracker restore leaves, is no longer reported as having "changes that
  render will discard": a re-render loses nothing in it. With the lane
  fixed, `undo` would have printed that after restoring such a file;
  `tracker checkout` and `tracker pull` already returned it, in warnings
  the CLI does not print.
