- `undo` waits for what it restarted and exits by whether the instance is
  up, as `spawn` and `start` do. It used to restart a database and return
  at once, so SQL run straight after it got "connection refused", and exit
  0 over an instance that was not up. It now ends with the same `ready:`,
  `starting:` or `not ready:` lines and exits 1 when the instance is not
  ready, including one a restore left as it found it (a `prepare` that
  failed before the undo). `spawn --from-checkpoint`, which finishes with
  an undo, waits too, and its `--json` gains `starting` and `problems`.
