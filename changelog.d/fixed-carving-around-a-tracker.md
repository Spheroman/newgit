- A tracker can no longer be carved around another, and an export no longer
  loses the files of a tracker mounted inside its root by something else.

  Carved around `lib`, `a` would not hold it, so a view rooted at `a`, and an
  export of it, silently lacked `apps/a/lib`. Both now refuse, naming the
  tracker. Found by a new fuzz test of compose and split, which any change to
  either can run harder with `NEWGIT_FUZZ_SEEDS=500`.
