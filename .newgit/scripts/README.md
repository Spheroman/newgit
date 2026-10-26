# .newgit/scripts/

Scripts that resource definitions shell out to. Reference one as
`{{scripts}}/<name>` in any resource command:

```toml
[actions.prepare]
command = "{{scripts}}/db-up.sh {{branch.slug}}"
```

`{{scripts}}` resolves to this directory in the **store** — the repository
you ran `newgit init` in — not to a copy inside the workspace. That is the
same rule the resource definitions in `../resources/` already follow, so both
halves of a definition live under one rule: edit either one and the next
`newgit action` picks it up, with nothing to commit first.

Commit this directory. It is control plane, like `config.toml`, `trackers/`,
and `resources/`, and a teammate or CI without these scripts cannot bind
your resources.

Scripts run with the workspace as their working directory, so a relative
path inside one refers to the instance being prepared. Mark them executable
(`chmod +x`), or invoke them through an interpreter in the command.

A script your *project* owns — something the app itself runs — belongs in the
project tree as usual, not here. Those are read from the workspace and must
be committed before the first spawn that calls them.
