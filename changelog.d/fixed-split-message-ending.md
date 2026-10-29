- A commit split into trackers comes back from newgit.dev with the id it
  was made with, when it was unsigned.

  Composing a split commit back always ended its message with a newline.
  GitHub writes merge commits without one, so everything cloned from
  newgit.dev had a new id from the first such merge on, and looked
  diverged from the same history on GitHub. This fixes commits split from
  now on; ones already split lost their ending when they were, and keep
  their rebuilt ids. Signed commits still come back with new ids, since a
  rebuilt commit cannot carry the original's signature.
