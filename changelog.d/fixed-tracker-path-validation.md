- Tracker paths are checked by where they lead rather than how they are
  spelled, and a code tracker may own other trackers' definitions: the
  governance layering docs/design/trackers.md describes under "Who may change
  what".

  The check read a path's first component as written, so `./.git`, which
  leads with `.`, got past the guard that refuses `.git`: a secret tracker
  could claim the workspace's Git directory, and its captures would sweep
  up the files in it. `tracker track` also kept `./.env.local` as written
  and ignored it as `/./.env.local`, which Git matches to no file, so the
  secret was not ignored at all. Paths are now normalized before they are
  checked and stored, and `.git` and `.newgit` are refused in any case,
  since a case-insensitive file system, macOS's default, opens `.GIT` as
  `.git`. A definition that already claims such a path no longer loads,
  and the error names the path.

  The same guard refused every claim inside `.newgit/`, including the one
  the design rests on: who may change a tracker's paths and audience is
  whoever may change the tracker owning its definition file, so the
  maintainers of an admin tracker owning `.newgit/trackers/web.toml`
  decide who reads `web`. The remote resolves such a claim, but locally
  the project would not open. Those files are all that opens up, and only
  to code trackers, since a secret tracker would keep the definition out
  of Git. The rest of `.newgit/` stays with `project` and `access`.
