- Code trackers are carved in the order they nest: `project` (`.newgit`)
  before `access` (`.newgit/trackers`), which carves out of it. By name,
  `access` came first and found nothing to carve, so a project whose
  history already held `.newgit/` (a clone adopted with `newgit init`)
  failed `remote add` with "refusing to merge unrelated histories".
