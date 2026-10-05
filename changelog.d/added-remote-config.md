- `.newgit/config.toml` accepts a `[remote]` section (`url`, optional
  `project`) naming the newgit remote a project syncs to. Nothing syncs yet.
  `url` must be https (plain http only for localhost), and
  `tracker create --storage remote` is refused when no `[remote]` is
  configured instead of quietly behaving as local.
