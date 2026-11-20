- `newgit action --quiet`, and a failed hook says the command it ran
  ([#89](https://github.com/Spheroman/newgit/issues/89)).

  A failing `prepare` could print a hundred thousand lines through
  `newgit action`, and the part that mattered was the rendered command
  at the top of the log and the end of it. `--quiet` keeps a one-shot
  command's output in its log and prints the exit code, plus, on
  failure, the command that ran and the log's last twenty lines. Spawn's
  `prepare: FAILED` line now shows the command too. A tool that tears
  itself down on failure can still empty its own log; the reference and
  the `supabase` template say how to rerun it by hand with its
  keep-it-up flag (`supabase start --ignore-health-check`).
