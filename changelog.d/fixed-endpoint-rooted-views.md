- A view rooted at a child tracker (`/<owner>/<name>/<child>.git`) now
  composes the same commits whatever the endpoint composed before it.

  The Worker's endpoint container runs this code, so newgit.dev served
  rooted views, such as `t3code`'s, that either failed with a 500 (when
  root's view had been composed first, the child's history was walked from
  its one-commit-deep snapshots and ended at the cut) or lacked a nested
  tracker's content at older commits (its pins were looked up as root's,
  `repos` rather than `t3code__repos`, so they were never fetched). Views
  already cached on newgit.dev stay wrong until their cache is cleared.
