- The install store keys a tree on the trackers its instance binds, so an
  instance is no longer filled with a tree built for a different binding:
  one that drops a tracker got the tree built for an instance that binds
  it, and the other way round.

  A shared lockfile lists every member, bound or not, and its install
  builds the bound ones, named by `{{bound.paths}}` or found on disk, where
  a dropped tracker's files are not checked out. The key held the identity
  paths' content, `key_command`'s output and the definition, and a shared
  lockfile does not change with the binding, so the tree built for one
  binding filled instances of another. docs/design/trackers.md already said the
  bound set was part of the install's identity.

  The binding is in every resource's key, not only those whose commands
  mention `{{bound…}}`, because an install can read it without naming it:
  an unfiltered `npm ci` links a workspace member only when the member is
  checked out, and a check for the placeholder would have left that case
  as it was. The cost is cache hits. Instances whose bindings differ no
  longer share a tree for any resource, even one the binding cannot
  change, such as a package's own install in its own tracker, so each
  binding installs once. Instances that bind the same trackers still
  share.

  Every existing store entry is re-keyed, which costs one reinstall per
  key: the first spawn at each new key installs and stores its tree, and
  `newgit cleanup` reclaims the old entries, which nothing keys to any
  more.
