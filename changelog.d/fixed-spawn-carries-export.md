- Spawning an instance that binds a tracker with its own export, such as
  t3code, takes about 3s instead of 21–25s, and its workspace no longer
  needs its own 400 MB of Git objects.

  Each workspace fetched the export target's branch, which is that
  tracker's whole history (236,000 objects for t3code), so that
  `gh pr create --fill` could list a pull request's commits. That fetch
  packed the whole history again for every spawn and wrote it as new
  files that no other workspace shared. Now the macOS source template
  fetches the branch once, at the cost of only what is new each time. Its
  objects reach the workspace through the same copy-on-write clone as
  everything else, so the workspace's own fetch has nothing to move.
