- `[identity]` is now the single declaration of what a resource is derived
  from, and `[checkpoint] paths` is gone
  ([#45](https://github.com/Spheroman/newgit/issues/45),
  [#46](https://github.com/Spheroman/newgit/issues/46),
  [#41](https://github.com/Spheroman/newgit/issues/41)).

  The reference described these three keys as one mechanism — identity feeds
  the checkpoint, the checkpoint feeds the restore — and the mechanism did not
  exist. `[identity].paths` was read in exactly one place, to put the word
  `identity` in a column of `newgit resource list`. `hash:<rev>` was written at
  checkpoint and parsed back nowhere. `recompute` re-ran its action without
  consulting either. Definitions worked only because their authors dutifully
  typed the same path list into two blocks; diverge them and nothing said so.
  The shipped `pnpm` template duplicated the list too.

  ```diff
  [identity]
  paths = ["package.json", "pnpm-lock.yaml"]

  [checkpoint]
  mode = "hash"
  -paths = ["package.json", "pnpm-lock.yaml"]
  ```

  `mode = "hash"` now hashes `[identity] paths` and has no path list of its
  own; it fails to load without an `[identity]` to hash. Identity paths are
  validated the way tracker paths always were — workspace-relative, no `..`,
  no reaching into `.git` or `.newgit`. Previously `paths = ["/etc/passwd",
  "../../escape"]` loaded clean.

  And `recompute` consults the hash, which is the point of recording it:

  ```
  resource: deps recompute(prepare) skipped: identity unchanged
  ```

  A monorepo `npm ci` is around ninety seconds, and three debugging cycles on
  an unrelated resource's restore command used to cost three of them. The
  comparison is between the checkpoint and the *pre-undo* state, not the
  workspace as it stands when the resource is reached — undo restores source
  first, so hashing at that point would compare the checkpoint against itself
  and skip every time, including the one case that matters: a lockfile that
  moved after the checkpoint and has just been rewound underneath a tree built
  from the newer one.

  Identity describes the inputs, not the tree, so the repair path stays
  reachable: `newgit undo --force-recompute` rebuilds regardless.
