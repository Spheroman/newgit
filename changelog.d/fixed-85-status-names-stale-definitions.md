- `newgit status` names every way a live instance differs from its
  resource definitions, with the commands that apply it
  ([#85](https://github.com/Spheroman/newgit/issues/85)).

  An instance bound before a definition was edited silently kept the old
  shape: an export, a `[[render]]`, a `prepare`, a removed port, or the
  running command all went unsaid, so an agent iterating on a definition
  debugged a stale binding. `status` now holds each ready binding against
  the definition (re-resolving exports and renders, and comparing the
  `prepare` that ran and the command the process started with) and prints
  one line per resource ending in `newgit resource rebind`, wrapped in
  `newgit stop` / `newgit start` when its process runs, or a stop and start
  alone for a changed command. What newgit reads live (`[ready]`,
  `depends_on`, hooks) is not flagged. A resource defined since the
  instance bound, and a binding whose definition was deleted by hand, are
  named too. The line gives the definition's absolute path: definitions
  are read from the project root, not a workspace. `status --json` is new.
