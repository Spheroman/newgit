- Restoring a deposit-only tracker (one that owns no paths, like
  `db-snapshots`) no longer writes its files into the workspace root; its
  content reaches resources through `{{state_ref}}` only. `newgit remote add`
  keeps a `[remote] project` already in `config.toml` instead of falling back
  to the local project name. Both found by running the flow against the live
  remote.
