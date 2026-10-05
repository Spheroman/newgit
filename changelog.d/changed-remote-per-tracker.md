- The remote stores code the way the machine does: one Artifacts repo per
  code tracker (`<owner>.<project>.t.<tracker>`), and an instance forks a
  tracker only the first time it changes it (`<owner>.<project>.i.<instance>.<tracker>`).
  The trunk repo and the whole-source fork per instance are gone; spawning
  forks nothing.
- Projects are `<owner>/<name>`, like GitHub. `newgit remote add` creates
  one, seeds each code tracker's repo, and creates branch `main`: a list of
  one commit per code tracker, which moves only through `newgit merge`.
  `--project <owner>/<name>` joins an existing one.
- A checkpoint sync pushes each changed tracker's branch and the
  checkpoint's commit to the instance's fork of it, and the record carries
  the checkpoint as lists. `spawn --from-checkpoint` composes those lists
  from the trackers you may read — the author's exact commits when you can
  read what they could. A `git push` on a remote succeeds only when that
  sync reached it.
- `newgit clone <owner>/<name>` composes a project's default branch from
  the code trackers you may read and sets newgit up around it.
- A workspace's `origin` is the project's Git endpoint,
  `https://newgit.dev/<owner>/<name>.git`; the credential helper answers it
  with the workspace's own token.
