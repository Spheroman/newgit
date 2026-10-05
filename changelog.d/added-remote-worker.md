- `remote/` is the newgit remote: a Cloudflare Worker with one Durable
  Object per project. It logs users in with GitHub's device flow, keeps
  members, instances, and checkpoints, forks an Artifacts repo per instance,
  and mints short-lived Git credentials by rule — trunk is never writable,
  a fork only by its owner, a tracker only by its audience as committed on
  trunk, and a per-user tracker only by that user.
