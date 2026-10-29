- The design documents moved from the repository root into `docs/design/`:
  `spec.md` (was `newgit-v1-mvp.md`), `trackers.md`, `remote.md` (was
  `newgit-cloudflare.md`), `vision.md` (was `newgit-architecture.md`), and
  `hosts.md`. `spec.md` is rewritten as the design of newgit as it is
  rather than as a v1 plan, and leaves every key and template variable to
  `newgit reference`. `vision.md` is labelled as the original vision, most
  of it unbuilt. The tracker and remote docs, the README, and the guide are
  brought in line with the code and list the open issues they know of.
  `newgit reference` documents `[remote]` in `config.toml` and your own
  `~/.newgit/config.toml`, shows sample tracker files, and says that
  `merge_with_source` is stored but not yet read.
