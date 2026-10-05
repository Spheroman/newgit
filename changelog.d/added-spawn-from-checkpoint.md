- `newgit spawn <name> --from-checkpoint <instance>/<checkpoint>` spawns a
  checkpoint from the project's remote — anyone's, an agent's included — and
  it comes up as it was: source with its uncommitted work, shared tracker
  content, and resource state such as a database restored from its dump.
  Per-user trackers resolve to *your* content, never the author's, and a
  tracker you may not read is left out and named. In a linked project,
  every checkpoint syncs its source, its remote-stored tracker revs, and
  then its record to the remote; a failed sync is a warning, not a failed
  checkpoint. `spawn --json` prints what happened for agents.
