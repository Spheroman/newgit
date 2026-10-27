- `newgit export --remote <name>` brings in what was merged on the target
  before it pushes. When the target's branch is ahead of the export (a pull
  request merged on GitHub, a dependabot bump), the project root
  fast-forwards to it and it merges into `main`, as `git pull` then
  `newgit merge main` did by hand. Before, the push was refused as not a
  fast-forward. When the target's commits are not the project's own (the
  project root has commits of its own, or the target withholds a tracker),
  it refuses and says how to bring them in. `--dry-run` reports how many
  commits would come in; `--force` still replaces the branch instead.
