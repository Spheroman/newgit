- `newgit serve` lands what is merged on a GitHub mirror, and exports to
  its mirrors ([#414](https://github.com/Spheroman/newgit/issues/414)).

  `POST /v1/webhooks/github?project=<owner>/<name>` verifies each event
  against the project's own secret, queues a push in SQLite (a restart
  resumes it), and a background thread lands what the mirror has beyond
  `main` as a merge: the same `merge` path a member's merge takes, as one
  landing, by the rules of whoever pushed there. A target rooted at a
  tracker (`t3code`) lands in that tracker and moves its pin in the same
  landing. Whose work a commit is, for a tracker the mirror withholds, is
  read from what the commits contain, never from a branch's name: a pull
  request from a branch named like an instance with unrelated withheld
  work lands (#451). A later pull carrying a refused one's commits is said
  once, on the first. A squash's pull request is read from GitHub's
  `pull_request` event, or asked; while GitHub cannot say, the pull waits.

  After every landing each target's view is pushed to its mirror, with
  the same ids the client's `newgit export` composes: a pull followed by
  an export is a fast-forward there. Exports never block a landing, are
  retried with backoff, never overwrite a mirror, are checked commit by
  commit to carry nothing of a withheld tracker or one outside Git, and
  say how they stand (up to date, behind, failing and why) in `newgit
  status`. Configure both with `newgit serve --github <file>`.
