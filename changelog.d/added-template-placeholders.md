- `newgit resource add` lists the lines of a template that are yours to
  fill in, and `resource check`, `spawn` and `status` report a placeholder
  left in any field as exactly that.

  `resource check` on an unedited `install` template said `` `EDIT` — not
  found on PATH ``, which reads as a missing program, and said nothing of
  the `EDIT ME` in `[identity] paths` and `produces`. Every bundled
  template's placeholder is now spelled `EDIT ME` or `EDIT_ME_*`, including
  the `external` template's stand-in CLI (`EDIT_ME_CLI`, was `cloudctl`).
