- A dev remote for trying remote changes before they merge: `newgit-dev`
  on workers.dev, with its own Artifacts namespace, Durable Objects, and
  container.
  - `remote/scripts/dev.sh` deploys the working tree in about a minute. It
    reuses prod's newest container image, so it needs no Docker.
  - `remote/scripts/dev-login.sh` logs in without a browser, through a
    route that only the dev remote answers.
