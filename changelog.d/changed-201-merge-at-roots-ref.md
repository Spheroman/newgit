- `newgit merge` makes the merge commit in your view and commits it at
  root's ref ([#201](https://github.com/Spheroman/newgit/issues/201)).

  A merge used to be made tracker by tracker and recorded as a list in the
  remote's Durable Object. Now a merge is a view commit split like any
  other: each tracker's files are merged by `git merge-tree` (a conflict
  moves nothing and names each file's tracker), the root commit moves by
  one compare-and-swap, and each child's `main` catches up with what root
  pins. A signed merge commit of yours comes back as itself. Before the
  push, the remote asks Git, in its endpoint's container, how each child's
  pin moves: forward along that child's `main` needs root's maintainers,
  anything else needs the child's; and before it writes the new index, it
  checks root's pushed commit pins exactly that. A merge into a branch
  other than `main` keeps a child's commit on the child's branch of that
  name and moves no child's `main`. On the
  remote a branch is root's ref, and the list is an index written after it
  moves. Merge stacking by tracker (`--only`) is not coming: with the root
  as spine, a partial merge would be a cherry-pick with new ids.
