- `newgit pull` with no arguments brings every mounted tracker up to date
  ([#345](https://github.com/Spheroman/newgit/issues/345)), as `nix flake
  update` does: each tracker in Git that root mounts moves its pin to its
  `follow` ref, in one commit, `Update trackers: t3code (14 commits),
  ui-kit (2 commits)`, instead of one `newgit pull <tracker>` per tracker.
  The commit lands on the branch it ran on (in the project root, its own
  instance branch, which `newgit merge main` publishes), so `main` and
  other instances keep their pins until it merges. A tracker the remote
  won't let you read is named and skipped rather than failing the rest; if
  none can be read, it fails rather than report everything up to date.
  Nothing behind, no commit. A pin holding commits its ref lacks stays put
  rather than lose them, and trackers outside Git are left alone: they
  have no pins.
