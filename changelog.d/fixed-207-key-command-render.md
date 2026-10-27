- `[identity] key_command` is rendered before it runs, like an action
  `command`, and the reference's *Template variables* tables list
  `{{bound.trackers}}` and `{{bound.paths}}`
  ([#207](https://github.com/Spheroman/newgit/issues/207)).

  The reference said every command is rendered, but `key_command` was
  handed to the shell verbatim, so `{{bound.paths}}` in one printed as
  `{{bound.paths}}`. It now gets the scope an action command gets: branch,
  workspace, scripts, this resource's ports and `{{bound.*}}`. The binding
  is already part of every key; `{{bound.paths}}` lets the command pass it
  to what it inspects. The reference now warns against printing
  per-instance values like `{{workspace}}`, which would key every instance
  apart. The two `bound` variables were described everywhere but the
  tables that are supposed to list every variable and where it has a
  value; they now have rows and a scope column, and the scope table has a
  `key_command` row.
