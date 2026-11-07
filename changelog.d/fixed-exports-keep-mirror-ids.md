- A branch exported from a workspace keeps the ids of what was merged on a
  mirror.

  A commit merged on GitHub lands on the remote rebuilt, without GitHub's
  signature, under a root commit naming the original. A workspace that pulled
  it split it again, losing that, so every later commit had ids the mirror had
  never seen, and GitHub showed its pull request as conflicting. Instances now
  learn what the remote made of `main` before splitting, using the workspace's
  own token, and say so if they can't.
