- `newgit reference` documents `[[render]]`. The feature shipped in 0.2.0 but
  the reference did not learn about it, so the one copy of the definition
  format guaranteed to be wherever the binary is was the one place it was
  missing — exactly the gap `newgit reference` exists to close. The section
  covers `path`, `replace`, `find`/`with`/`count`, and the four rules; the
  template-variable scope table gains a `[[render]] with` row, and the note on
  unresolved placeholders now names `[[render]]` alongside `[cleanup]` as the
  other place they refuse rather than render verbatim.
