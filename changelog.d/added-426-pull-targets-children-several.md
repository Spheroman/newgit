- `newgit pull` now updates everything "update" means
  ([#426](https://github.com/Spheroman/newgit/issues/426)):
  - A tracker exported to a target rooted at it (t3code →
    Spheroman/t3code) first takes what is new there, as `newgit pull
    --remote` would: a fast-forward of its `follow` ref, or a real merge.
    That is one more fetch per such tracker. The pin moves on the branch it
    ran on; the tracker's `main` moves only when `newgit merge` lands it,
    in an instance and in the project root alike. A target that can't be
    reached, or conflicts, is named and skipped with everything below it;
    the other trackers still move.
  - Trackers mounted inside another are pulled too: a commit in the
    tracker that mounts them (`Update trackers: repos (1 commit)`), whose
    pin then moves, up to root. `newgit pull t3code/repos` does it for one.
    A child of a tracker the instance doesn't bind stays, and is named
    when it was asked for or is behind.
  - Every argument is a tracker: `newgit pull t3code ui-kit` pulls those
    two. The instance moved to `--instance <name>` (it is still inferred in
    a workspace); naming an instance where a tracker goes says so and gives
    the `--instance` form. A named tracker that stays exits non-zero.
  - Two instances pulling the same two diverged tips make the same merge
    commit (author, committer, and dates from its parents), so merging both
    lands one merge, not three.
  - `newgit pull <tracker>` writes `Update trackers: <tracker> (N
    commits)`, like plain `newgit pull`, and no longer moves a pin holding
    the instance's own commits back to its `follow` ref: that pin stays
    and is named.
