- A push that defines a new secret tracker with `storage = "remote"` is no
  longer refused. The remote reads a tracker's audience from its definition
  on the default branch, and only that push would put it there, so the
  checkpoint's sync used to fail on the tracker and take the whole push
  with it. Now a tracker the remote has no definition for is left out of
  that sync, with a warning that says why and `remote: synced to …,
  without` it, and the first checkpoint after the definition lands brings
  it. The remote answers 404 for such a tracker, and a real refusal (403)
  still fails the push.
