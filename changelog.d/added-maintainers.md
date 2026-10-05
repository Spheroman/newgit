- `.newgit/trackers/` is its own code tracker, `access`, created by `newgit
  init` with `maintainers = "owner"`: only the project's owner changes who may
  read what, while anyone who merges may change resources, scripts, and
  config (the `project` tracker).
- Trackers take an optional `maintainers` key — who may merge changes to them
  on a remote (`owner`, `project-devs`, `user:<login>`); by default, anyone
  who may read a tracker may change it. Changing a tracker's definition is
  changing the tracker that owns its file in `.newgit/trackers/`, so
  governance layers: a tracker can own other trackers' definitions, and only
  its maintainers redefine them. The remote and the Git endpoint read each
  definition from the tracker that owns its file.
- An instance reports whether it works, checked rather than remembered:
  `status` says `ready` or `not ready` with the reasons (a resource that did
  not come up, a tree it builds that is missing, a running resource's port
  not answering), and `spawn` ends with the same verdict. A resource's
  `prepare` runs quietly during `spawn`; a failure shows its last lines and
  its log.
- A checkpoint no longer aborts on a resource that is not up and whose
  capture fails: it goes ahead without it, marked degraded. `undo` rebuilds
  a resource's tree when it is missing, even if its inputs did not change.
- `spawn` says plainly that nothing leaves the machine until `git push`.
