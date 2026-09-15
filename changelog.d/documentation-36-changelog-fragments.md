- Changelog entries are written as one file per change in `changelog.d/`
  and folded into `CHANGELOG.md` at release time
  ([#36](https://github.com/Spheroman/newgit/issues/36)), and AGENTS.md
  records the two other conventions that keep parallel branches from
  queueing behind each other
  ([#37](https://github.com/Spheroman/newgit/issues/37)).

  Nothing about the entries themselves changes: they are still written when
  the change is made, still explain why rather than what, and are still the
  reason release notes are not generated from PR titles. Only where they are
  parked until release moved. Six branches appending prose to one
  `## [Unreleased]` section meant every branch conflicted with every branch
  that landed before it — four of seven rebases during the #23–#28 series
  hit `CHANGELOG.md`, and for three of them it was the only conflict, each
  resolved by the same mechanical act of keeping both entries in either
  order. Different filenames cannot conflict, so that queue is gone by
  construction rather than by being handled well.
