- `newgit remote delete <owner>/<name>` deletes a project from its newgit
  remote: every token pinned to it is revoked, every repo under its prefix
  deleted, and its record cleared, so the name can be linked again from
  scratch. Owner only, from a login rather than an agent's or workspace's
  token, and the project is typed out to confirm. It removes `[remote]` from
  `.newgit/config.toml`.
