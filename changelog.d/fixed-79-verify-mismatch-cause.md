- `checkpoint --verify` now says why a resource MISMATCHed — a `[restore]`
  that landed elsewhere, or a `[checkpoint]` that is not deterministic — and
  shows what differs between the two deposits
  ([#79](https://github.com/Spheroman/newgit/issues/79)).

  The two causes need opposite fixes, and two opaque revs could not tell
  them apart: a `pg_dump` checkpoint stamps every dump with a fresh random
  `\restrict` token, so a correct restore was reported as a failed one. On
  a mismatch, verify now checkpoints that resource once more with nothing
  restored in between: if that reproduces the second ref, `[restore]` is at
  fault; if not, `[checkpoint]` is. For `into_tracker` deposits it lists the
  changed files and line counts, shows the first changed lines, and prints
  the `git diff --no-index` command over both snapshots. A mismatch still
  fails the verify — this is evidence, not a looser check.
