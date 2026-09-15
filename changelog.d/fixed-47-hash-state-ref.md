- A `hash` checkpoint's state ref can no longer reach a command as an
  argument ([#47](https://github.com/Spheroman/newgit/issues/47),
  [#48](https://github.com/Spheroman/newgit/issues/48)).

  `hash` records the content hash of `[identity] paths`. That answers one
  question — did the inputs move — and it never names a concrete thing to
  restore or tear down. But both places that resolve `{{state_ref}}` fell back
  to the recorded ref whatever it was, so a restore command got
  `restore-from hash:0fa284b468` and, worse, a cleanup hook got
  `delete-environment hash:0fa284b468` and *ran* it. The refusal that exists
  for exactly this case only asked whether a placeholder was still
  unresolved, so it caught the argument that was missing and not the one that
  was wrong.

  A hash ref now resolves to nothing at all. `{{state_ref}}` stays verbatim in
  a restore command, where an unresolved placeholder is already how a mistake
  is made visible, and the cleanup guard fires unchanged — one rule about what
  a state ref *is*, rather than a second check bolted beside the first.

  What the current definition *can* decide is refused when it loads.
  `[checkpoint]` and `[restore]` are two halves of one mechanism but were
  validated one section at a time, so every pairing loaded. Now a
  `{{state_ref}}` under a checkpoint that can never record one — `hash`, or
  `none`, or no `[checkpoint]` at all — fails at load, in a `[cleanup]`
  command as well as a `[restore]` one, since learning at teardown that the
  hook never ran is the worse half of the same mistake. The runtime guard
  still stands behind it, for the case reading the current file cannot
  predict: a record written under an older definition.

  These refusals are about the *placeholder*, not the mode. A restore command
  that never asks for a state ref is an ordinary rebuild whatever the
  checkpoint records, and still loads; so do pairings that are merely inert,
  like a `command` checkpoint under a `recompute` restore. The one mode
  pairing refused outright is `external` + `recompute`, which is not inert:
  `recompute` re-runs `prepare`, which for an external resource mints a
  second instance and orphans the one the handle names.
