- After a merge below root, the next merge from the same instance no longer
  conflicts with `main`. Root's roll-up kept a client's root commit only if
  it had one parent, so a client's merge commit was replaced by one of the
  roll-up's own. The merger's history was left off `main`, and the next
  merge three-wayed against an old base. Root now keeps the merge commit.
  - Instead, the remote checks every commit a merge brings to a tracker
    above its anchor, not only the end result. A root file edited and then
    edited back makes the change root's to approve.
