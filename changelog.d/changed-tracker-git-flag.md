- A tracker definition says whether it is in Git with `git`, not whether it
  is secret with `secret`; `newgit tracker create --code` is now `--git`.

  There is one kind of tracker. `secret` mixed two things: whether the
  content is committed with Git, and how sensitive it is. Sensitivity is the
  `audience`; what is left is where the content lives, and plenty of content
  that is not sensitive belongs outside Git too (agent notes, a public page,
  db snapshots). `git = true` is what `secret = false` was; the default,
  `git = false`, keeps today's fail-closed behaviour, and the key is written
  only when `true`. Behaviour does not change, but a definition still saying
  `secret` is refused like any unknown key, so rewrite `secret = false` as
  `git = true` in `.newgit/trackers/*.toml` (and delete `secret = true`).
  `newgit tracker list` shows a `GIT` column in place of `KIND`.
