- Tracker definitions take `role`, `priority`, `requires`, and `resources`.
  Trackers in a role are alternatives and may claim the same paths; a spawn
  binds one per role — the one `--use` names, else your own default
  (`[projects.<project>.roles]` in `~/.newgit/config.toml`), else the highest
  priority; a negative priority means only when asked. Binding a tracker
  binds what it `requires`, and a required tracker cannot be dropped.
  Resources a tracker brings bind only while it is bound, and start and stop
  with `newgit tracker use` and `drop`; `use` swaps a role's tracker.
  `spawn --use` / `--drop`.
- `{{bound.trackers}}` and `{{bound.paths}}` give a resource's commands what
  the instance binds, so an install can build only the bound members.
- `tracker list` is a tree by ownership, with each tracker's kind, role,
  priority, and maintainers; inside a workspace it marks what is bound.
