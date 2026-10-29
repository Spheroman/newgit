- A `[[render]]` can target a file inside its resource's own `[identity]
  produces`, so the install store can share a tree that names its own
  workspace.

  Until now the store declined any tree that mentioned the absolute path of
  the workspace it was built in, and pnpm writes that path into
  `node_modules/.pnpm-workspace-state-v1.json`, so no pnpm workspace was
  ever stored. A render into a built tree treats the file as rendered for
  the instance that built it: storing reverses it to `find`, and filling
  renders it forwards with the filled instance's `{{workspace}}`. The stored
  copy is still scanned, so a path the render does not cover still keeps
  the tree out. `count` is refused on such a render, and `render --check`
  skips it. The `pnpm` template now declares one for the workspace state
  file. On T3 Code, a second `newgit spawn` now takes about 5s instead of
  50s.
