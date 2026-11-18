- `newgit resource check` fails a definition that names something that is
  not there, and `spawn` and `status` name it, with the command that fixes
  it, for every instance that binds it.

  A `[checkpoint] into_tracker` naming a tracker nobody created passed
  `resource check`, `spawn` and `status`, and failed only at the first
  `checkpoint`. The same went for `{{ports.x}}` naming no port,
  `{{exports.X}}` naming no export or used where it has no value,
  `{{scripts}}/x` with no such script in the project root, `{{state_ref}}`
  or `{{snapshot.path}}` outside the hooks that give them a value, a
  misspelled `{{branch.*}}`, and a tracker's `resources` naming no
  resource: each ran with the literal text or was silently skipped. They
  are read from the definitions in one place now. `resource check` also
  fails what the graph refuses (`depends_on` naming nothing, a cycle, an
  environment name claimed twice), and `--json` reports all of it under
  `definitions`.
