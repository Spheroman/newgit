- A failed hook shows its whole command, `--quiet` keeps a command's
  input, and a failed capture is not echoed
  ([#89](https://github.com/Spheroman/newgit/issues/89)).

  A multi-line command printed only its first line after `ran:`, and the
  rest turned up in the `|` lines as though the command had printed it;
  the log now marks every line of the command, and `ran:` and `--dry-run`
  show all of it. `newgit action --quiet` gave the command no input, so
  `echo yes | newgit action db.ask --quiet` failed where it passed
  without the flag; it now reads what was piped to newgit, with captures
  or without. A failed action with `captures` showed its stdout, the
  captured values and any password in them, on the terminal; the tail is
  now stderr only, and the log keeps stdout apart under a marker. Without
  `--quiet`, such a failure showed nothing but its exit code, since its
  output never reaches the terminal; it now shows the command and that
  stderr tail too. newgit itself destroys none of a failed hook's
  evidence, and a test now holds it to that: each run has its own log, a
  retry included, and nothing newgit runs after a failure removes what
  the command left.
