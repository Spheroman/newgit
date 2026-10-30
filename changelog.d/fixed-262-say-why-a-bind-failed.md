- `spawn` and `status` say why a resource failed to render or to resolve an
  export, and no longer suggest an action it does not have
  ([#262](https://github.com/Spheroman/newgit/issues/262)).

  The reason was only in `spawn --json`, and the retry hint always named
  `<resource>.prepare`, which a resource without one refuses. The error is
  now kept in the binding record and printed by `status` and under the
  last line of a failed `spawn`, which used to promise "see below" with
  nothing below it. The hint follows the failure: a render or export
  points at the file or the definition and the new `newgit resource
  rebind <resource> [instance]`, which binds it again as spawn did; any
  other hint names only an action the resource defines.
