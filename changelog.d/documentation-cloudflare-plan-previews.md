- `newgit-cloudflare.md` now centers on `spawn --from-checkpoint` and
  trackers: spawning someone else's checkpoint resolves per-user trackers to
  your own content, and previews run in one sandbox per distinct
  environment, started from the checkpoint's container snapshot, a CI
  snapshot, or an environment snapshot keyed on resource versions.
