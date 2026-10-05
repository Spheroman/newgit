- Second-round review fixes:
  - The secret gate checks every commit actually pushed — each tracker's
    `main` too, which can hold work merged here that the project root's
    branch never had — against the history `newgit init` adopted (or a clone
    received), recorded once as `refs/newgit/adopted`.
  - A merge moves each tracker's branch on the remote only from the commit it
    started from (`--force-with-lease`), so a merge that stalled past its
    lock cannot rewind one that landed meanwhile. Merge write passwords last
    no longer than the lock and are cancelled when the merge finishes or a
    later merge takes an expired lock; the trackers a merge may move are
    recorded with its lock. A merge with nothing to move is refused.
  - A merge that succeeded is reported as succeeded even when bringing the
    project root along fails; `remote add` on an empty repository says to
    commit something first.
