- `newgit init` where `.newgit/` already exists adopts it instead of
  refusing. A plain `git clone` of a newgit project, or a project set up
  before trackers had histories, left `root` with no history, so `remote
  add`, `merge main`, and `export --remote` failed with "its history shares
  no commit with the trackers' histories". `init` now writes any missing
  built-in tracker definitions and gives `root` the project's history, as a
  fresh `init` does; that error names it.
