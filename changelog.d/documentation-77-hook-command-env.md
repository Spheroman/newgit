- The reference now states that `[checkpoint]`, `[restore]` and `[cleanup]`
  receive the full command environment
  ([#77](https://github.com/Spheroman/newgit/issues/77)).

  They always did — all three call the same `assemble_env` an action does —
  but every mention of the environment was scoped to "`newgit run` and
  actions", and the same section states plainly that those three hooks *are
  not actions*. Between those two facts the reference could not answer
  whether a `[restore]` sees `[exports]` and `[ports.<name>] env`, and the
  omission read as deliberate because the neighbouring template-variable
  scope table resolves the same question for `{{...}}`.

  That is the worst hook to leave unanswered. A `[restore]` that resets a
  database picks *which* database from the port newgit allocated; without
  that variable it falls back to the committed default, which is usually the
  developer's shared local database. Discovering the contract empirically
  means running a destructive command against live state to see what
  survives.

  There is now an environment scope table beside the template-variable one,
  `[exports]` and `[ports.<name>] env` say "every command newgit runs"
  instead of naming actions, and the "not actions" sentence says what it is
  and is not a claim about — `workdir` and invocability, not the
  environment. Unlike `{{...}}`, the environment does not vary by site:
  there is one environment and every command gets all of it. A new test
  (`tests/hook_env.rs`) asserts all three hooks see another resource's
  export, a port `env`, and `NEWGIT_*`, so the documented claim cannot drift
  from the code.
