- A commit split into trackers comes back from newgit.dev, a clone, or
  `spawn --from-checkpoint` as the commit it was made as, signature and id
  included, for anyone who can read every tracker it touched.

  Composing a split commit back always ended its message with a newline,
  and never had the original's signature to put back. GitHub writes merge
  commits without a final newline and signs them, so everything cloned
  from newgit.dev had a new id from the first such merge on, and looked
  diverged from the same history on GitHub. Split commits now keep the
  original's message ending and its headers in their trailers, and a
  rebuilt commit takes the signature back only when its id comes out as
  the original's, so a signature only ever sits on what it covers. Commits
  split before this fix keep their rebuilt ids until their history is
  repaired.
