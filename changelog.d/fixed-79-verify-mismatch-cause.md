- `checkpoint --verify` now says why a resource MISMATCHed — a `[restore]`
  that most likely landed elsewhere, or a `[checkpoint]` that is not
  deterministic — and shows what differs between the deposits
  ([#79](https://github.com/Spheroman/newgit/issues/79)).

  The two causes need opposite fixes, and two opaque revs could not tell
  them apart: a `pg_dump` checkpoint stamps every dump with a fresh random
  `\restrict` token, so a correct restore was reported as a failed one. On
  a mismatch, verify now checkpoints that resource once more with nothing
  restored in between: if that differs from the second ref, `[checkpoint]`
  is at fault; if it matches, `[restore]` most likely is. For
  `into_tracker` deposits it lists the changed files and line counts, shows
  the first changed lines with control characters escaped, and prints the
  `git diff --no-index` command for the rest — comparing `after` against
  the control when the checkpoint is the problem, since every line there is
  noise. The diff stays bounded for a dump of any size. A mismatch still
  fails the verify — this is evidence, not a looser check.
