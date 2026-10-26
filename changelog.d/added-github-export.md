- `newgit export --remote <name>` pushes the trackers' `main`s, composed
  with their history, to an ordinary Git repository such as a GitHub mirror,
  defined in `.newgit/exports/<name>.toml`.

  Nobody moves their code off GitHub to try a tool, so newgit has to sit
  beside it: a project can live in newgit while its GitHub repository, and
  everything that reads it (`gh`, Actions, clones), keeps working. What
  ships fails closed: `public` code trackers, plus the ones a target names
  in `trackers`, so a public mirror and a private one can show different
  views. A secret tracker cannot be named, and a history that ever added a
  secret tracker's path is refused before anything is pushed. Pushing uses
  plain `git push` with your own credentials; exporting again
  fast-forwards, and a rewritten history (a narrower tracker set) needs
  `--force`.
