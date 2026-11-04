- `newgit merge` makes the merge commit in your view and commits it at
  root's ref ([#201](https://github.com/Spheroman/newgit/issues/201)).

  A merge used to be made tracker by tracker and recorded as a list in the
  remote's Durable Object. Now a merge is a view commit split like any
  other: each tracker's files are merged by `git merge-tree` (a conflict
  moves nothing and names each file's tracker), the root commit moves by
  one compare-and-swap, and each child's `main` catches up with what root
  pins. A signed merge commit of yours comes back as itself. Before the
  push, the Worker is told which trackers' pins move to a commit not on
  their own `main` and requires their maintainers and root's; the Worker
  cannot run Git, so it takes that list from the client, and the Git
  endpoint serves only pins found in each tracker's own repo. On the
  remote a branch is root's ref, and the list is an index written after it
  moves. Merge stacking by tracker (`--only`) is not coming: with the root
  as spine, a partial merge would be a cherry-pick with new ids.
