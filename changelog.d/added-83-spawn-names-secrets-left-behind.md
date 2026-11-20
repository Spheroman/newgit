- `newgit spawn` names the local secrets it left behind
  ([#83](https://github.com/Spheroman/newgit/issues/83)).

  A workspace is made from source, so a gitignored `.env.local` or
  `config/master.key` in the project root never reached it unless a
  tracker carried it, and nothing said so: every resource came up `ok`,
  and the service that read the file failed hours later. `spawn` now
  names each such file (ignored in the root, present there, named like a
  secret, and missing from the new workspace) with the commands that
  carry it, `tracker create`, `track`, and `capture --from-store`, and
  `spawn --json` lists them under `left_behind`. It is a note: nothing is
  copied. The reference has a *Local secrets* section with the recipe.
