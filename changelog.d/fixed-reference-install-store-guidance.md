- `newgit reference` recommends a content-addressed package manager, in the
  one document that travels with the binary. Per-instance installs are the
  single place newgit multiplies a cost instead of absorbing it, and the
  choice that decides how much — pnpm or npm — is made once, early, by someone
  who has usually not read the README's section on it by then. The reference
  had a parenthetical `(a pnpm store)` in the ownership table and nothing
  else. It now says it plainly, with the per-tool costs and the two keys that
  wire a shared store up (`ownership = "user"`, `[identity] paths`).
