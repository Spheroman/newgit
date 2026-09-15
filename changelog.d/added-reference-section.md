- `newgit reference [section]` prints one section instead of all 380 lines,
  and bare `newgit reference` now prints a table of contents.

  The reference grew past the point where paging the whole thing to find
  `[ports]` was reasonable. `newgit reference render`, `newgit reference
  ownership`, `newgit reference tracker` — plural forms and unambiguous
  prefixes resolve too, so `trackers` and `template` land where you meant. An
  ambiguous prefix says which sections it matched rather than dumping the
  list. `newgit reference all` is the old behavior, byte for byte.

  Sections are derived from the document's own headings rather than listed in
  the CLI, so a section added to `definitions.md` becomes addressable without
  touching Rust and the two cannot drift. Asking for a `##` section brings its
  `###` subsections with it, so `resource` is the whole resource format.
