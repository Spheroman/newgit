- On macOS the install store clones a stored tree with one `clonefile(2)`
  call per produced path instead of `cp -c`.

  `cp -c` clones one file at a time, so on a large pnpm monorepo a cache
  hit was slower than the install it replaced: filling T3 Code's 264k-file
  `node_modules` took 37s, against 17s for `pnpm install`. A cache-hit
  `newgit spawn` of that repo now takes about 5s instead of 40s. `cp` is
  still the fallback, and Linux keeps `cp --reflink`.
