- Security fixes from review of the remote:
  - Tracker definitions decide every audience, so they are the project
    owner's: only the owner merges changes to the `project` tracker, adds
    code tracker repos, or writes the `project` tracker's repo. A member can
    no longer widen a secret tracker's audience by any route.
  - The Git endpoint composes a commit for a tracker only if that tracker's
    own repo has it; a branch naming another tracker's commit is left out,
    and cannot take the endpoint down with it.
  - Merge passwords require read access to a code tracker; a merge may move
    only the trackers it declared; branches are created by people only.
  - One secret gate before everything sent to the remote: sync, push,
    `newgit merge`, `newgit merge main`, and `remote add`'s first
    publication. A merge refuses when the instance's work did not sync.
- `spawn --from-checkpoint` works for a checkpoint taken after a merge.
- Merges and `remote add` can be retried after failing partway; `remote add`
  publishes `main` after merging the project root into it, so local merges
  are kept.
- A merge into `main` fast-forwards the project root when it can, so the next
  spawn starts from the merged work.
- `newgit clone` leaves trackers you cannot read absent rather than inventing
  their history; importing a checkpoint warns about trackers not defined
  here.
