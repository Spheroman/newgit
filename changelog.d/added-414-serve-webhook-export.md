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

  The server talks to a mirror only at
  `https://github.com/<owner>/<name>.git` (or where the file's `git` key
  says GitHub's Git is), and sends the project's token to no other host:
  a definition's `url` as written is never handed to Git there, mirror
  Git reads no global or system Git config (so no `insteadOf` redirects
  it), and a pull whose target no longer names the repository its event
  did fetches nothing. A fetch from a mirror offers it only the view's
  commits as what the server has, never a tracker's own history, checks
  what it receives (`fetch.fsckObjects`), and is stopped after ten
  minutes; `/state` says pulls are stalled when the pull thread is gone
  or stops moving. Its own export coming back as an event is answered
  without a pull. A later pull refused for a reason of its own says that
  reason instead of repeating the first refusal's; a refusal is never
  pushed out of `newgit status` by the pulls waiting on it or by another
  mirror's; a refused pull sent again that lands while later pulls
  waited on it leaves those said, on their own, rather than dropping
  them; and it ends when the export next reaches the mirror, so `newgit
  pull --remote <target>` then `newgit merge main` clears it without an
  event.
