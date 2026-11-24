# Development

This repository uses [`mise`](https://mise.jdx.dev/) to pin Rust.

```sh
mise install
mise run ci        # fmt-check + clippy -D warnings + test, exactly what CI runs
mise run test
mise run install   # build and install into ~/.cargo/bin
```

CI runs the suite on Linux and macOS, checks the declared MSRV, and smoke
tests the release binary against a throwaway project — the tests here drive
real `git`, real process groups, and real ports, so they are integration
tests by nature. On a pull request macOS runs a subset, the tests that
exercise what differs there; main runs all of it. `.config/nextest.toml`
lists what the subset skips, and `cargo nextest run --profile macos-pr` runs
it locally.

## Workspace layout

- `crates/newgit-core` holds the domain model: branch instances, tracker
  definitions and their histories, the compose/split engine that turns
  tracker histories into one workspace history and back, resource
  definitions and lifecycle hooks, metadata storage, real-directory
  materialization, checkpoints and undo, export, cleanup, and the remote
  client.
- `crates/newgit` is the CLI, published as the `newgit` crate.
- `crates/newgit-serve` is `newgit serve`: the remote as one process, with
  Git repositories on disk and project state in SQLite.
- `remote/` is the hosted remote: a Cloudflare Worker with Durable Objects
  over Artifacts repos, deployed at `https://newgit.dev`. Try a change to it
  on the dev remote before it merges: `remote/scripts/dev.sh` deploys the
  working tree there (`--build` builds the container too, with Docker), and
  `remote/scripts/dev-login.sh` logs in to it without a browser.
- `.newgit/` is created by `newgit init`. Committed: `config.toml`,
  `trackers/`, `resources/`, `scripts/`. Gitignored local state: `branches/`,
  `checkpoints/`, `logs/`, `state/`, `installs/`, and `local/` — which holds
  `trackers.git`, every tracker's history (bare repo).
