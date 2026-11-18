- `undo` recomputes what depends on a tree it just rebuilt or refilled.
  Setting up Remix + Prisma, an undo refilled `node_modules` from the
  install store, skipped `prisma-client` as "identity unchanged", printed
  `ok`, and the app crashed on `Cannot find module '.prisma/client'`: a
  tree rebuilt from its inputs holds nothing a dependent generated into it.
  Now every `recompute` dependent of a resource the undo recomputed runs
  again, and its line says which dependency it followed. `start` after
  `stop --evict`, which restores through the same undo, does the same.
  The reference says a resource that generates into another's tree
  declares `depends_on` on it and names its output in `produces`.
