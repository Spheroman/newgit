- A checkpoint and a merge make fewer, faster requests to the remote.
  - Every password a checkpoint or merge needs comes in one request
    (`POST /credentials` with `repos`), minted at once.
  - The remote reads tracker definitions in parallel and keeps them until a
    definition holder's `main` moves.
  - A checkpoint moves the instance's head in the same request.
  - `newgit` asks who you are and reads the project once per run.
  - A merge's finish answers as soon as the list lands. The branches follow
    after the answer, and what can't catch up is logged rather than shown.
  - A new instance's fork of root starts when the instance is made, so its
    first checkpoint finds the fork made or under way instead of waiting
    for the copy.
