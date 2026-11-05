- A GitHub webhook brings what is merged on a mirror into `main` on the
  remote: `POST /v1/webhooks/github`, verified against
  `GITHUB_WEBHOOK_SECRET`
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  Merging a pull request on GitHub used to wait for someone to run `newgit
  pull --remote`. Now the endpoint's container does what that does, under
  `main`'s merge lock, with the same refusals, and lands it as a merge by
  whoever pushed. A mirror's commit keeps its id through the round trip,
  signature included, so the next export is a no-op, and an event for our
  own export takes no lock. A commit that may hold an instance's work in a
  withheld tracker is refused rather than landed without it: `newgit merge
  <instance>`, then redeliver. Set-up is in
  `docs/design/remote.md`, *The GitHub webhook*.
