- The `project` tracker is gone: `.newgit/` is ordinary content of `root`,
  and `access` is mounted at `.newgit/trackers`
  ([#201](https://github.com/Spheroman/newgit/issues/201)).

  `project` existed only to hold `.newgit/` apart from the code, and with
  mounts there is no reason to: whoever may merge root may change its
  resources and scripts, as before. Exporting `root` therefore ships
  `.newgit/` (config, resources, scripts, export targets); only
  `.newgit/trackers/` follows `access`'s audience, so an export target that
  mirrored `"project"` should drop it. `{{bound.trackers}}` no longer hides
  `project`. `newgit init` writes `root` and `access`; an existing project
  runs `newgit tracker migrate`.
