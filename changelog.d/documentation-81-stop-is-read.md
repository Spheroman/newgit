- The reference no longer contradicts itself about whether `stop` is a
  reserved action name
  ([#81](https://github.com/Spheroman/newgit/issues/81)).

  The `signal` row documented a default of "`term` for a `stop` action", and
  three paragraphs later the same section said action names carry no
  meaning — "`start`/`stop` are a convention, made real by `long_running`
  and `signal`, not by the names". The code agrees with the first one:
  `stop_signal()` is a literal `actions.get("stop")`.

  `stop` is now documented as the one name newgit reads, and for exactly one
  purpose: when newgit stops a supervised process on its own — during
  `remove`, `undo`, and `resource remove --force` — it signals, and reads
  `[actions.stop] signal` to choose which signal, defaulting to `term`.

  The part the issue could not determine from outside is now stated and
  tested. An action named `stop` **with a `command`** is accepted and is an
  ordinary action: invoking it runs the command like any other. But newgit's
  own stops always signal and never run an action's command, so such an
  action is never reached that way and the resource gets a bare `term`. A
  teardown that is a command rather than a signal (`docker compose stop`)
  belongs in `[cleanup]`, which newgit does run.
